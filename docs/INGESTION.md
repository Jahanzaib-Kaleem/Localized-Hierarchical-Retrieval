# CSV ingestion and append

LHR has separate concepts for **creating a dataset**, **appending rows to an existing dataset**, and **combining independent buckets**. They intentionally do not share ambiguous UI labels.

## Create from CSV

An empty bucket can be initialized from a CSV.

Studio:

1. choose an empty bucket;
2. choose the CSV;
3. review the inferred schema;
4. create the dataset.

The strict Rust importer is two-pass and bounded-memory. Dictionary values are externally sorted using bounded runs, canonical rows are encoded in bounded batches, exact indexes are built on disk, the staged generation is verified/sealed, and only then is `CURRENT` atomically replaced.

`import_csv_initial_with_progress` performs the empty-bucket check while holding the catalog writer lock. Two concurrent initial-import jobs therefore cannot both initialize the same bucket.

## Append CSV

A populated bucket uses **Append CSV** rather than replacement semantics.

Append requires the incoming schema to be compatible by column name:

- no missing columns;
- no extra columns;
- logical types must match;
- nullable settings must match;
- normalization rules must match;
- explicit null literals must match;
- CSV column order may differ because headers are mapped by name.

Append is implemented as a new immutable indexed delta layer. LHR hard-links the currently published generation into staging, builds/indexes only the incoming CSV, assigns new monotonically increasing logical row IDs, adds the new delta to `overlay.json`, and publishes the resulting generation atomically.

This avoids rewriting every existing row merely to append new rows. Existing rows and indexes remain immutable. If append fails at schema validation, parsing, building, indexing, verification, or publication, the previous `CURRENT` generation remains the visible dataset.

## Large Studio uploads

Studio does not send a multi-gigabyte CSV as one browser request. It uses the resumable import-job API:

```text
POST /v1/admin/imports
PUT  /v1/admin/imports/{id}/chunk?offset=...
POST /v1/admin/imports/{id}/complete
GET  /v1/admin/imports/{id}
```

The browser uploads sequential **4 MiB chunks**. Each HTTP request is therefore bounded independently of total CSV size. Authenticated admin chunk requests do not consume the ordinary per-principal query/control-plane request-per-minute bucket; otherwise the chunk protocol itself would cap multi-GB throughput. Chunks remain protected by authentication, the global concurrency ceiling, sequential byte offsets, the fixed per-request chunk ceiling, disk preflight, and the aggregate import-size ceiling. Upload state is persisted under:

```text
<service-root>/temp/import-jobs/
```

and bytes are staged under:

```text
<service-root>/temp/studio-uploads/
```

With the normal appliance root at `/data`, both live on the persistent data volume rather than the container/boot filesystem.

The default aggregate CSV ceiling is currently 64 GiB (`max_import_bytes`) and is an operator resource/abuse limit, not a requirement that the file fit in RAM. The ordinary JSON/request body limit remains separate.

### Resume safety

Studio stores the active job ID in browser `sessionStorage`. A page reload can reconnect to a queued/running server-side job. If a reload interrupts the browser upload, the user can re-select the same local file and Studio resumes from the server's persisted byte offset.

Name and size are not considered sufficient identity. Studio hashes only the first 1 MiB with SHA-256 (bounded browser memory), and the server verifies that fingerprint from the first uploaded chunk. A different same-name/same-size file is therefore rejected rather than spliced onto a partial upload.

A chunk whose response is lost is also safe to retry: Studio first asks the server for the persisted byte offset. The server accepts only the next sequential offset and persists progress only after the chunk has been flushed and synced.

## Import job stages

Import status is durable enough for Studio to reconnect and exposes real stages rather than fabricated percentages:

```text
uploading
queued
validating
parsing
building
indexing
publishing
complete
failed
```

During upload, Studio can show exact bytes and percentage because the total file size is known. During the first CSV pass, LHR persists real parsed-row progress every 65,536 rows; later server-side stages report their stage without inventing a denominator. It does not invent an ETA or a percentage for work whose denominator is unknown.

A normal failure records the backend error and whether the previous dataset was preserved. The upload file is removed after a completed or failed build.

If the service restarts while a job is already building, immutable publication still guarantees that the bucket is either on the old complete generation or the new complete generation. Because the process may have crashed just after publication but before final job-status persistence, the recovered job deliberately says to inspect `CURRENT` before retrying rather than claiming that retry is automatically safe.

Incomplete uploads can survive an ordinary service restart and remain resumable. Abandoned uploading jobs expire after 24 hours; an hourly cleanup task marks them failed and removes their staged file. Chunk writes for one job are serialized, and if the server cannot persist the new byte offset after writing a chunk it truncates the upload back to the previously durable offset before returning an error.

## Duplicate behavior

Duplicates are **allowed by default**. Create and append preserve repeated rows because duplicate records can be legitimate data.

Automatic full-row or key-based deduplication is not enabled in this release. Future uniqueness modes should remain explicit (for example exact-row or selected-column keys with a declared conflict policy) rather than silently changing import semantics.

## Combine is different from append

Append extends one existing bucket with compatible rows and preserves its identity.

Bucket combine intentionally merges one or more independent source buckets into a **new** target bucket and leaves the sources unchanged. See [BUCKETS.md](BUCKETS.md).

## Memory and disk behavior

The architectural requirement is bounded application memory as file size grows, not zero RAM use.

Expected scaling:

```text
CSV size grows
  -> upload/build time grows
  -> temporary/final disk grows
  -> application buffers remain bounded
```

The strict importer still performs disk-backed dictionary sorting and bounded row batches. Studio chunks are fixed-size. Append builds only the incoming delta rather than materializing the existing bucket in RAM.

Peak disk usage during create includes the staged upload plus the generation being built. During append it includes the upload plus the incoming delta build and staging metadata. Before accepting a job, the service requires a conservative free-space floor of four times the declared CSV size plus 64 MiB. This is a safety floor, not a promise that every data distribution will fit: dictionary/index amplification can still require more. Existing generation files are normally hard-linked within the same data filesystem; on a filesystem where hard-linking is unavailable the existing clone helper can fall back to copying, which increases peak disk requirements.

## Reverse proxies

Chunking removes the need for a proxy to accept one enormous request. A proxy still needs to allow individual ~4 MiB request bodies plus normal overhead and must not impose an unusually short request timeout.

When diagnosing transport failures, isolate layers in order:

1. service locally on the VPS;
2. direct private/host port;
3. Nginx or another reverse proxy;
4. Cloudflare or another external tunnel/proxy.

Do not attribute an ingestion failure to the outer proxy until the same request path is known to work at the inner layer.

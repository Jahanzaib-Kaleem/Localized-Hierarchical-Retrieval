# CSV ingestion and append

LHR has separate concepts for **creating a dataset**, **appending rows to an existing dataset**, and **combining independent buckets**. They intentionally do not share ambiguous UI labels.

## Create from CSV

An empty bucket can be initialized from a CSV.

Studio:

1. choose an empty bucket;
2. choose the CSV;
3. review the inferred schema;
4. create the dataset.

The low-level Rust importer is two-pass and bounded-memory. Studio import jobs now place that engine behind a **segmented bulk-ingest layer** instead of asking one engine build to absorb an arbitrarily large CSV. The Studio path reads the source sequentially, seals bounded internal parts (default limits: 1,000,000 rows or about 512 MiB of decoded CSV payload), builds each part independently, attaches later parts as immutable internal delta layers, verifies the complete versioned dataset, and only then atomically replaces `CURRENT`.

This means import memory and temporary index-spool size are bounded by an individual part rather than the total file size. A 60 GiB CSV therefore increases the number of immutable parts and total build time; it does not turn into one 60 GiB dictionary/index build. The part engine additionally caps dictionary sort runs at 16 MiB by default for Studio jobs.

CSV headers define column identity. A shorter data record is treated as having missing trailing fields: those cells become NULL and the affected columns are automatically widened to nullable in the published schema. A record containing more fields than the header is still rejected because those extra cells have no safe column names to map to.

`import_csv_initial_with_progress` performs the empty-bucket check while holding the catalog writer lock. Two concurrent initial-import jobs therefore cannot both initialize the same bucket.

## Append CSV

A populated bucket uses **Append CSV** rather than replacement semantics.

Append uses **automatic additive schema evolution by column name**:

- CSV column order may differ;
- existing columns may be absent from the new file; those values read as NULL for the appended rows;
- newly named columns are added to the logical schema automatically; older rows read NULL for those columns;
- shared columns must keep the same logical type, normalization rule, and explicit null-literal semantics;
- nullability may widen when a column is absent from a layer or a shorter CSV record;
- unnamed cells beyond the CSV header are rejected rather than guessed.

The versioned reader exposes the deterministic union of the base and delta schemas. Columns missing from a physical layer are projected as NULL, so adding a column does not rewrite millions of existing rows. Streaming compaction later materializes that union into a new clean base generation.

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

Name and size are not considered sufficient identity. The server hashes the first 1 MiB of the first uploaded chunk with SHA-256 and persists that fingerprint. When Studio resumes, it re-sends only the first bounded chunk at offset zero for identity verification; the server compares the first 1 MiB and returns the already-durable offset without appending those bytes again. This avoids depending on browser Web Crypto or a secure HTTPS origin while still preventing a different same-name/same-size file from being spliced onto a partial upload.

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

Append extends one existing bucket with rows matched by column name and preserves its identity; additive/missing columns evolve that bucket's logical schema automatically.

Bucket combine intentionally merges one or more independent source buckets into a **new** target bucket and leaves the sources unchanged. See [BUCKETS.md](BUCKETS.md).

## Memory and disk behavior

The architectural requirement is bounded application memory as file size grows, not zero RAM use.

Expected scaling:

```text
CSV size grows
  -> number of bounded immutable parts grows
  -> upload/build time grows
  -> final disk grows
  -> per-part application memory/index spool remains bounded
```

Studio chunks are fixed-size. The segmented builder writes only one bounded CSV part at a time, invokes the existing exact engine for that part, attaches the verified result to the unpublished generation, removes the temporary part workspace, and continues. Append similarly adds only new bounded parts; existing published rows remain hard-linked and immutable.

For disposable Studio upload files on Linux, LHR attempts `FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE` after a part has been successfully absorbed into the unpublished generation. On ext4 and other supporting filesystems this releases blocks belonging to already-consumed source ranges while preserving the logical file offset used by the CSV reader. Failure to punch a hole is treated as an optimization miss, not data corruption; the per-part free-space guard continues to protect the build.

The admission floor is therefore no longer four complete copies of the whole CSV. It reserves roughly two source-file sizes plus bounded part workspace (currently four 512 MiB part budgets plus 64 MiB). Before every part build LHR also re-checks free space and requires four times that actual part file plus 64 MiB. These are conservative guards, not guarantees: unusual dictionary/index amplification can still exhaust a filesystem, in which case the unpublished stage is abandoned and the previous `CURRENT` remains unchanged.

Existing generation files are normally hard-linked within the same data filesystem; on a filesystem where hard-linking is unavailable the clone helper can fall back to copying, which increases peak disk requirements.

## Reverse proxies

Chunking removes the need for a proxy to accept one enormous request. A proxy still needs to allow individual ~4 MiB request bodies plus normal overhead and must not impose an unusually short request timeout.

When diagnosing transport failures, isolate layers in order:

1. service locally on the VPS;
2. direct private/host port;
3. Nginx or another reverse proxy;
4. Cloudflare or another external tunnel/proxy.

Do not attribute an ingestion failure to the outer proxy until the same request path is known to work at the inner layer.

## On-demand large-file benchmark

Large upload tests are intentionally **not** part of ordinary pull-request CI because generating and importing 50 MB through 1+ GB fixtures on every change would consume substantial runner minutes and storage.

After starting the release candidate service, run the end-to-end Studio import-job protocol locally on the target host:

```bash
export LHR_BASE_URL=http://127.0.0.1:8787
export LHR_API_TOKEN='...'

python3 scripts/benchmark_import_jobs.py --sizes-mb 50 250 700 1024

# Crunchbase-like width: base 5 columns + 36 extra text columns = 41 total
python3 scripts/benchmark_import_jobs.py --sizes-mb 700 1024 --extra-text-columns 36
```

This creates a separate temporary bucket for each size, generates the CSV incrementally under `/data/temp/import-benchmarks`, uploads it in the same 4 MiB chunks as Studio, polls real server stages, samples RSS/page faults/process I/O and free disk, verifies final row count plus exact lookups, prints JSON reports, and deletes the benchmark buckets/files by default. It never touches the default/Apollo bucket.

To exercise repeated append instead of independent creates:

```bash
python3 scripts/benchmark_import_jobs.py --append-parts-mb 250 250 250 250
```

The first part creates the temporary dataset and every later part uses append. IDs remain unique across parts and every published row count is verified. Add `--keep-buckets` or `--keep-files` only when artifacts are deliberately needed for debugging.

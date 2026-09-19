# LHR HTTP Service

`lhr serve` exposes the operational database through an authenticated HTTP API while keeping the Rust storage engine usable as a standalone library/CLI.

## Security model

The safe default is loopback-only HTTP (`127.0.0.1:8787`). A non-loopback bind is rejected unless `behind_tls_proxy` is explicitly enabled. That flag is an operator assertion that TLS is terminated by a trusted reverse proxy or that an equivalently protected private transport is being used.

Remote listeners also require at least one API key. Keys have one of three roles:

- `read`: bucket discovery, query, stats, workload, generations, metrics;
- `write`: all read operations plus row mutations and selected-row bucket transfers;
- `admin`: all operations including imports, bucket management/combine, compaction, vacuum, recovery, and index changes.

Bearer tokens are hashed before lookup and are never written to telemetry or audit logs.

Example configuration:

```json
{
  "bind": "127.0.0.1:8787",
  "api_keys": [
    {"id": "reader", "token": "replace-with-a-long-random-token", "role": "read"},
    {"id": "writer", "token": "replace-with-another-long-random-token", "role": "write"},
    {"id": "admin", "token": "replace-with-a-third-long-random-token", "role": "admin"}
  ],
  "max_body_bytes": 8388608,
  "max_import_bytes": 68719476736,
  "max_concurrent_requests": 64,
  "rate_limit_per_minute": 600,
  "max_query_limit": 10000,
  "max_rows_examined": 5000000,
  "max_query_timeout_ms": 30000,
  "max_mutation_ops": 100000,
  "max_batch_rows": 65536,
  "max_sort_records": 1000000,
  "max_dictionary_run_bytes": 268435456,
  "import_part_rows": 1000000,
  "import_part_bytes": 536870912,
  "behind_tls_proxy": false
}
```

Start it with:

```bash
lhr --root /data/my-db serve --config service.json
```

For a reverse-proxy deployment, bind LHR to a private/loopback listener whenever possible. If the proxy requires a non-loopback internal bind, set `behind_tls_proxy: true` only after TLS and network access controls are actually in place.

## Endpoints

### Liveness / readiness

- `GET /healthz` — process liveness; intentionally unauthenticated for local orchestrators.
- `GET /readyz` — reports ready when at least one bucket contains a readable published dataset; an empty fresh appliance remains `not_ready` while Studio/control-plane routes stay available.

### Read API

- `GET /v1/buckets` — list the reserved default bucket and named bucket workspaces.
- `POST /v1/query` — bucket-aware typed exact query protocol.
- `GET /v1/stats?bucket=<id>` — schema/index/storage statistics.
- `GET /v1/schema?bucket=<id>` — exact logical schema including type, nullability, normalization and null literals.
- `GET /v1/workload?bucket=<id>` — persistent workload telemetry, latency percentiles, and index recommendations.
- `GET /v1/generations?bucket=<id>` — immutable generation catalog.
- `GET /metrics` — Prometheus text exposition including aggregate bucket gauges.

### Write API

- `POST /v1/mutate` — bucket-aware atomic insert/update/delete transaction implemented as an immutable delta layer plus visibility/tombstone changes.
- `POST /v1/buckets/transfer` — copy or move selected stable logical row IDs between buckets.

### Administrative API

- `POST /v1/admin/import/csv` — legacy single-request streamed multipart CSV import retained for compatibility.
- `POST /v1/admin/imports` — create a durable Studio import job in explicit `create` or `append` mode.
- `PUT /v1/admin/imports/{id}/chunk?offset=<bytes>` — append the next bounded 4 MiB upload chunk.
- `POST /v1/admin/imports/{id}/complete` — finish upload and start the server-side build.
- `GET /v1/admin/imports/{id}` — reconnect to durable upload/build/progress/error state.
- `POST /v1/admin/compact`
- `POST /v1/admin/vacuum`
- `POST /v1/admin/recover`
- `POST /v1/admin/index/add`
- `POST /v1/admin/index/drop`
- `POST /v1/admin/index/rebuild`
- `POST /v1/admin/buckets/create`
- `POST /v1/admin/buckets/rename`
- `POST /v1/admin/buckets/delete`
- `POST /v1/admin/buckets/combine`

Filesystem-path operations such as arbitrary backup/restore destinations are intentionally kept out of the network API. They remain local administrative CLI operations so a compromised HTTP credential cannot be turned directly into arbitrary filesystem reads/writes.

### Bucket selection

Dataset-specific operations default to the reserved `default` bucket when no bucket is supplied.

JSON request bodies such as `/v1/query`, `/v1/mutate`, compaction, vacuum, and index administration carry a `bucket` field. Read metadata endpoints use the `?bucket=<id>` query parameter. Studio CSV multipart import carries a `bucket` form field.

Named buckets are independent catalogs rather than table namespaces inside one physical generation. See [`BUCKETS.md`](BUCKETS.md) for create/delete/combine/transfer semantics.

### Studio CSV import jobs

Studio uses the import-job API rather than the legacy one-shot multipart route.

The browser previews at most a small sample, then uploads the CSV in sequential 4 MiB chunks. Job metadata and upload bytes are persisted below `<service-root>/temp/`, which is `/data/temp/` in the normal appliance. Total CSV bytes are checked against `max_import_bytes`, but HTTP request memory/body size is bounded by the per-chunk size rather than the whole CSV.

Jobs use explicit modes:

- `create`: the selected bucket must still be empty while the catalog writer lock is held;
- `append`: the selected bucket must be ready. Columns are matched by name; existing column semantics remain stable, missing columns read as NULL for the appended rows, and newly named columns extend the logical schema with NULLs in older layers.

Studio create and append jobs use bounded segmented bulk ingestion. The default part ceiling is 1,000,000 rows or about 512 MiB of decoded CSV payload, whichever comes first; operators can tune these with `import_part_rows` and `import_part_bytes`. Each part is built/indexed independently and attached to one unpublished immutable generation. A failure in any later part abandons the whole stage, so `CURRENT` never exposes a partially imported file.

On Linux, disposable Studio upload files also attempt sparse hole punching after a successfully absorbed part. This can return already-consumed source blocks to the filesystem while later parts are built. Failure to reclaim a range is safe: LHR keeps the source range allocated and the per-part free-space guard remains authoritative.

Status exposes `uploading`, `queued`, `validating`, `parsing`, `building`, `indexing`, `publishing`, `complete`, and `failed`. Upload byte counts are exact. Parsed-row counts are exposed when known; fake ETAs/percentages are not.

Studio stores the active job ID in browser session storage. A build survives page reload. An interrupted upload can resume after the same file is re-selected; a SHA-256 fingerprint of only its first 1 MiB is persisted and verified to prevent mixing two same-name/same-size files.

The legacy `POST /v1/admin/import/csv` endpoint remains for compatible clients and still streams multipart bytes to disk, but it does not provide the resilient chunk/reconnect protocol used by Studio.

See [INGESTION.md](INGESTION.md) for append atomicity, cleanup, duplicate behavior and reverse-proxy guidance.

## Query protocol

`POST /v1/query` accepts the same `QueryRequest` used by the Rust library and `lhr query-json`.

Example equality request:

```json
{
  "bucket": "default",
  "filters": [
    {"op": "eq", "column": "country", "value": "pk"},
    {"op": "eq", "column": "industry", "value": "biotech"}
  ],
  "select": ["email", "company", "country"],
  "limit": 100,
  "after_row_id": null,
  "max_rows_examined": 1000000,
  "timeout_ms": 5000
}
```

Supported exact predicates:

- equality (`eq`);
- set membership (`in`);
- inclusive numeric range (`range`) on signed/unsigned columns.

Pure equality queries retain the optimized LHR exact-index path. Narrow bounded integer ranges may reuse the exact singleton backbone when the bounded decomposition route is applicable. For mixed shapes, exact equality filters can drive a bounded candidate stream and residual range/set filters are evaluated only on those candidates. A full visible-row fallback remains for shapes with no usable exact candidate access path.

Pagination is based on stable logical row IDs (`after_row_id`), not physical row offsets. Compaction therefore does not invalidate the logical cursor ordering.

## Resource controls

The service enforces independent ceilings for:

- ordinary JSON/request body size;
- Studio CSV import bytes;
- concurrent requests;
- requests per API key per minute;
- query return limit;
- rows examined by a fallback query;
- query execution time;
- mutations per transaction;
- builder batch size;
- external-sort records;
- dictionary-sort memory budget;
- segmented-import row and byte ceilings per internal part.

Client-supplied limits can tighten these ceilings but cannot raise them.

## Audit log

Successful and failed mutation/admin actions are appended as JSON Lines. The default path is:

```text
<catalog>/audit/audit.jsonl
```

Entries include timestamp, request ID, API-key ID, action, success/failure, and non-secret operation details. Raw API tokens are never logged.

## Metrics

`GET /metrics` exposes process and database counters, including:

- HTTP requests/errors/active requests;
- auth failures and rate-limit rejections;
- query count/failures/cumulative query time;
- mutation, compaction, and admin-operation counters;
- RSS;
- minor/major page faults (Linux);
- process bytes read/written (Linux);
- active snapshot generations;
- bucket count and ready-bucket count;
- aggregate visible rows and aggregate bucket storage;
- legacy default-bucket row count and canonical/routing/total bytes.

Persistent per-query telemetry is separate from process counters. It records query shape, latency, rows examined, pages touched, hierarchy lookups, optimized-route use, and planner-selected exact indexes. `lhr workload` and `/v1/workload` aggregate this into P50/P95/P99 and candidate accelerator recommendations.

## Concurrency and snapshots

The database is generation-based. Readers pin a generation with a snapshot lease; publication of a new generation does not change an in-flight reader's view. Lease-aware vacuum will not remove a generation still held by an active reader. The HTTP service caches the opened `VersionedDataset` for each bucket and keys that cache by the resolved generation path, so datasets containing many immutable ingest parts do not reopen every dictionary/index mmap on every request; the cache swaps automatically after `CURRENT` changes.

Writers still obey each selected bucket catalog's single-writer publication lock. Named buckets have independent catalogs/locks, while multi-bucket transfer/combine operations preserve their own validation/publication rules. Expensive imports, mutations, compaction, recovery, and index administration run outside the async HTTP executor on blocking worker threads after any network upload has been streamed to disk.

## Graceful shutdown

`SIGINT`/Ctrl-C triggers graceful Axum shutdown. In-flight OS-level database operations complete according to the normal generation transaction guarantees; unpublished generation work is never made visible merely because the process received a shutdown signal.

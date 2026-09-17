# LHR HTTP Service

`lhr serve` exposes the operational database through an authenticated HTTP API while keeping the Rust storage engine usable as a standalone library/CLI.

## Security model

The safe default is loopback-only HTTP (`127.0.0.1:8787`). A non-loopback bind is rejected unless `behind_tls_proxy` is explicitly enabled. That flag is an operator assertion that TLS is terminated by a trusted reverse proxy or that an equivalently protected private transport is being used.

Remote listeners also require at least one API key. Keys have one of three roles:

- `read`: query, stats, workload, generations, metrics;
- `write`: all read operations plus mutations;
- `admin`: all operations including imports, compaction, vacuum, recovery, and index changes.

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
  "max_import_bytes": 536870912,
  "max_concurrent_requests": 64,
  "rate_limit_per_minute": 600,
  "max_query_limit": 10000,
  "max_rows_examined": 5000000,
  "max_query_timeout_ms": 30000,
  "max_mutation_ops": 100000,
  "max_batch_rows": 65536,
  "max_sort_records": 1000000,
  "max_dictionary_run_bytes": 268435456,
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
- `GET /readyz` — opens the logical dataset and reports whether the service is ready to answer queries.

### Read API

- `POST /v1/query` — typed exact query protocol.
- `GET /v1/stats` — schema/index/storage statistics.
- `GET /v1/workload` — persistent workload telemetry, latency percentiles, and index recommendations.
- `GET /v1/generations` — immutable generation catalog.
- `GET /metrics` — Prometheus text exposition.

### Write API

- `POST /v1/mutate` — atomic insert/update/delete transaction implemented as an immutable delta layer plus visibility/tombstone changes.

### Administrative API

- `POST /v1/admin/import/csv` — streamed multipart CSV import used by Studio; publishes a new immutable generation after the normal exact import/verification pipeline succeeds.
- `POST /v1/admin/compact`
- `POST /v1/admin/vacuum`
- `POST /v1/admin/recover`
- `POST /v1/admin/index/add`
- `POST /v1/admin/index/drop`
- `POST /v1/admin/index/rebuild`

Filesystem-path operations such as arbitrary backup/restore destinations are intentionally kept out of the network API. They remain local administrative CLI operations so a compromised HTTP credential cannot be turned directly into arbitrary filesystem reads/writes.

### Studio CSV import

`POST /v1/admin/import/csv` accepts multipart form data with exactly one CSV file plus a reviewed LHR schema JSON field. It requires an `admin` credential.

The upload path is deliberately disk-first:

1. the multipart file is consumed in chunks and written to `<catalog>/temp/studio-uploads/`;
2. bytes are counted against `max_import_bytes` while streaming;
3. the uploaded schema is parsed and validated as `LHR-SCHEMA/1`;
4. the temporary file is passed to the existing two-pass `import_csv` engine;
5. dictionary construction, exact-index construction, verification, sealing, and atomic publication follow the same rules as a CLI CSV import;
6. the temporary upload is removed after the build completes.

Studio uses a bounded browser-side sample only for preview and schema inference. The full CSV is not accumulated in browser state. Large imports that exceed the HTTP convenience limit should continue to use the local CLI rather than raising the network limit indiscriminately.

## Query protocol

`POST /v1/query` accepts the same `QueryRequest` used by the Rust library and `lhr query-json`.

Example equality request:

```json
{
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

Pure equality queries retain the optimized LHR exact-index path. Narrow bounded integer ranges may reuse the exact singleton backbone when the bounded decomposition route is applicable; wider/open-ended/mixed set/range shapes retain the deterministic versioned fallback until additional exact accelerators are justified.

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
- dictionary-sort memory budget.

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
- dataset row count and canonical/routing/total bytes.

Persistent per-query telemetry is separate from process counters. It records query shape, latency, rows examined, pages touched, hierarchy lookups, optimized-route use, and planner-selected exact indexes. `lhr workload` and `/v1/workload` aggregate this into P50/P95/P99 and candidate accelerator recommendations.

## Concurrency and snapshots

The database is generation-based. Readers pin a generation with a snapshot lease; publication of a new generation does not change an in-flight reader's view. Lease-aware vacuum will not remove a generation still held by an active reader.

Writers still obey the catalog's single-writer publication lock. Expensive imports, mutations, compaction, recovery, and index administration run outside the async HTTP executor on blocking worker threads after any network upload has been streamed to disk.

## Graceful shutdown

`SIGINT`/Ctrl-C triggers graceful Axum shutdown. In-flight OS-level database operations complete according to the normal generation transaction guarantees; unpublished generation work is never made visible merely because the process received a shutdown signal.

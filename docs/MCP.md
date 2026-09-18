# LHR MCP control plane

LHR ships a stateless Model Context Protocol (MCP) control plane for AI clients and operator tooling. It is part of the same Rust appliance as the database HTTP service and Studio: there is no Node MCP daemon and no second database process.

## Runtime layout

The container runs one `lhr-appliance` process:

- `8787` — LHR Studio + authenticated HTTP API
- `8788` — MCP over HTTP (`POST /mcp`)
- `/data` — the persistent LHR volume containing the reserved default catalog plus named bucket catalogs

The MCP listener and the ordinary API operate on the same bucket catalogs, immutable generations, writer locks, snapshot leases, schemas, indexes and telemetry files. Existing pre-bucket installations remain the reserved `default` bucket without a data rewrite.

The default Docker/Compose mappings bind both listeners to `127.0.0.1`. Do not expose either listener directly to the public Internet. For remote MCP clients, terminate HTTPS at a trusted reverse proxy/private tunnel and forward only the MCP route/listener that you intend to expose.

## Authentication and roles

MCP uses the same bearer API-key model as the LHR HTTP service:

```text
Authorization: Bearer <token>
```

Roles are ordered as:

```text
read < write < admin
```

`tools/list` is filtered by the authenticated role. A read credential cannot discover or invoke write/admin tools; a write credential can mutate rows but cannot run administrative maintenance; an admin credential can use the full control plane.

The one-command Docker setup generates an administrator token in the persistent volume at:

```text
/data/.lhr-admin-token
```

Retrieve it locally with:

```bash
docker exec lhr cat /data/.lhr-admin-token
```

Treat that token as a database administrator secret. For shared or externally reachable deployments, use dedicated API keys/roles rather than distributing one administrator credential to every client.

## MCP protocol

The endpoint is:

```text
POST http://127.0.0.1:8788/mcp
```

LHR supports the modern stateless MCP revision `2026-07-28` and the legacy `2025-11-25` handshake era for compatibility. Modern requests are validated against the `MCP-Protocol-Version`, `Mcp-Method`, and (for `tools/call`) `Mcp-Name` headers.

Modern clients can call `server/discover` and then `tools/list` / `tools/call` without a server-side protocol session. Tool-list results are private-cacheable for a short TTL because the visible catalog depends on the authenticated role.

## Read/query tools

### `lhr_query`

Runs the typed exact query API. It supports equality, set membership, signed/unsigned ranges, projection, stable logical-row cursors, row-examination ceilings, and timeouts. Server ceilings always win over values supplied by the client.

Typical lead query arguments:

```json
{
  "bucket": "default",
  "filters": [
    {"op": "eq", "column": "country", "value": "US"},
    {"op": "in", "column": "industry", "values": ["Biotech", "Pharmaceuticals"]}
  ],
  "select": ["email", "company", "industry", "country"],
  "limit": 100,
  "after_row_id": null
}
```

Use `next_cursor`/`after_row_id` rather than requesting huge result sets. An empty `filters` array is the database-browse operation and returns the next bounded page in stable logical-row order. Equality-only requests retain LHR's optimized exact-index route; other supported predicate families use deterministic bounded fallback when no dedicated exact accelerator exists.

All dataset-specific MCP tools accept an optional `bucket` argument. Omitting it selects the compatibility bucket `default`.

### `lhr_row`

Fetches one visible row by stable logical row ID.

### `lhr_schema`

Returns the active logical schema, including logical types, normalization and nullability.

### `lhr_stats`

Returns rows/pages, canonical/routing/total bytes, column cardinalities and current index metadata.

### `lhr_explain`

Explains equality routing across the immutable base and delta layers without executing a mutation.

### `lhr_workload`

Returns persisted query-shape telemetry, latency percentiles, index-use counts and workload-based accelerator recommendations.

### `lhr_generations`

Lists immutable generations and active reader leases.

### `lhr_indexes`

Lists the exact/routing index portfolio and representation/storage metadata.

### `lhr_diagnostics`

Returns operator diagnostics from the real LHR appliance process:

- process RSS
- minor/major page faults
- process disk read/write bytes
- filesystem total/available bytes
- current dataset storage status
- active snapshot generations
- MCP request/failure/tool/auth/rate counters
- configured query/concurrency/resource ceilings

This makes it possible for an AI/operator to inspect actual RAM and I/O behavior rather than infer it from application-level timings.

### `lhr_benchmark_query`

Runs a bounded query repeatedly against one pinned immutable snapshot and reports min/median/p95/p99/max latency plus process/page-fault deltas. Warmups and measured iterations are explicitly bounded. Benchmark calls do **not** pollute persisted workload telemetry.

### `lhr_verify`

Runs full structural/versioned verification. It is read-only but can be disk-intensive on a large database, so it should be used intentionally rather than as a frequent health probe.

## Write/admin tools

### `lhr_mutate` (`write`)

Applies insert/update/delete operations as one crash-safe immutable delta transaction using the same service resource ceilings.

### Bucket tools

- `lhr_buckets` (`read`) lists the default and named buckets with readiness, rows, columns and storage.
- `lhr_bucket_transfer_rows` (`write`) copies or moves selected logical rows between buckets. Empty destinations are initialized from the source schema; populated destinations must match that schema exactly.
- `lhr_bucket_create`, `lhr_bucket_rename`, `lhr_bucket_delete`, and `lhr_bucket_combine` (`admin`) manage bucket workspaces.

The reserved `default` bucket cannot be deleted. Its display name can be changed without rewriting its data.

### Administrative tools (`admin`)

- `lhr_compact`
- `lhr_vacuum`
- `lhr_recover`
- `lhr_index_add`
- `lhr_index_drop`
- `lhr_index_rebuild`
- `lhr_update_status`
- `lhr_update`

`lhr_update` does not receive Docker or host-root access. It writes a narrowly-scoped request under `/data/control`. On supported Linux/systemd installations, the setup script installs a root-owned host path watcher that consumes that request and runs the same persistent-data-safe installer used for manual upgrades. The MCP connection can briefly disconnect while the container is replaced.

The first release containing this feature must still be installed manually once so the host watcher exists. After that bootstrap, later published releases can be requested through admin MCP.

MCP write/admin operations are appended to `audit/mcp-audit.jsonl` in the LHR volume. Database writer locking and immutable publication semantics remain authoritative, so the MCP layer cannot bypass normal LHR consistency rules.

## Intentionally not exposed over MCP

Bulk file ingestion and filesystem backup/restore paths remain local CLI/operator operations. Exposing arbitrary server paths to a remotely connected model would turn an MCP credential into a general filesystem capability and would make very large uploads a poor fit for the protocol.

Use the existing local CLI for those operations:

```text
lhr import csv
lhr import external
lhr backup
lhr restore
```

Once data is inside LHR, MCP covers bucket selection/management, normal querying, row mutations, diagnostics, query benchmarking, index administration, generation maintenance, and a constrained software-update request on hosts where the updater was installed.

## Docker quick start

```bash
docker run -d \
  --name lhr \
  --restart unless-stopped \
  -p 127.0.0.1:8787:8787 \
  -p 127.0.0.1:8788:8788 \
  -v lhr-data:/data \
  ghcr.io/jahanzaib-kaleem/lhr:latest
```

Then:

```text
Studio: http://127.0.0.1:8787
MCP:    http://127.0.0.1:8788/mcp
```

For a remote AI client, place HTTPS/private transport in front of `8788` and configure the client with the remote `/mcp` URL plus the bearer credential. The database itself does not need a separate Postgres-style network protocol or another application server.

## OpenAI/API connection shape

A remote MCP-capable client only needs the HTTPS MCP URL and authorization header. Conceptually:

```json
{
  "type": "mcp",
  "server_label": "lhr",
  "server_url": "https://lhr.example.com/mcp",
  "headers": {
    "Authorization": "Bearer <LHR_API_TOKEN>"
  }
}
```

Do not commit real tokens to source control. Approval behavior should respect the tool annotations: read-only tools are marked read-only, while mutations and administrative operations are explicitly non-read-only/destructive where appropriate.

## Low-resource design notes

The MCP layer is designed not to change LHR's low-memory assumptions:

- one Rust OS process hosts API/Studio + MCP;
- MCP protocol state is stateless;
- tool lists are small metadata payloads;
- lead retrieval is bounded and cursor-paginated;
- query limits/timeouts/row-examination ceilings are enforced server-side;
- benchmark iteration counts are bounded;
- no result path is intended to materialize a database-sized response;
- database reads still use snapshot leases and mmap-backed LHR structures;
- writes/admin operations still use the existing immutable generation and writer-lock machinery.

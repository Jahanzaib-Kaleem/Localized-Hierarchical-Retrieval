# Localized Hierarchical Retrieval (LHR)

LHR is a deterministic exact database/retrieval system for very large structured datasets, designed around low query-time RAM, low data touched, and expensive-but-bounded preprocessing.

The project began from a practical question: can a 50M-100M+ row lead dataset remain fast and exact on unusually constrained hardware without depending on embeddings, semantic retrieval, or a large analytical database stack?

The answer evolved into a Rust storage engine plus a full operational database layer built around immutable generations, exact adaptive indexes, delta mutations, compaction, typed queries, recovery, observability, an industrial browser Studio, an authenticated HTTP service, and a first-class MCP operator control plane.

No LLM, embeddings, semantic similarity, or probabilistic retrieval is required. Values and columns have no inherent meaning to the engine.

## One-command quick start

Run the complete LHR appliance:

```bash
docker run -d \
  --name lhr \
  --restart unless-stopped \
  -p 127.0.0.1:8787:8787 \
  -p 127.0.0.1:8788:8788 \
  -v lhr-data:/data \
  ghcr.io/jahanzaib-kaleem/lhr:latest
```

The image contains the Rust database/service, compiled Studio assets, and MCP control plane. Node is used only while building the image; it is not a production runtime process.

Open:

```text
Studio: http://127.0.0.1:8787
MCP:    http://127.0.0.1:8788/mcp
```

The first run generates a persistent administrator token. Retrieve it locally with:

```bash
docker exec lhr cat /data/.lhr-admin-token
```

Use that credential in Studio or as the bearer credential for an explicitly trusted MCP client. The default Docker mappings are loopback-only. Remote API/MCP access should be placed behind trusted HTTPS/private transport rather than exposing either listener directly.

Docker Compose is also included:

```bash
docker compose up -d
```

## Current status

The production-oriented implementation lives under `rust/`. It now includes:

- canonical tokenized data stored once per physical layer;
- deterministic schema + mmap dictionaries;
- an exact singleton backbone for generic completeness;
- adaptive exact representations (`bitslice`, `deltapost`, `densepost`, `flatpost`, sparse postings);
- selective pair/multi-column accelerators;
- compressed streaming intersections;
- conservative page routing when exact row indexes do not fully cover a query;
- bounded-memory external sorting/building;
- immutable generation publication through atomic `CURRENT`;
- stable logical row IDs;
- append-only delta inserts/updates and tombstone deletes;
- versioned reads across base + deltas;
- streaming compaction;
- snapshot reader leases + lease-aware vacuum;
- SHA-256 integrity seals, verified backup/restore, rollback, and recovery;
- exact index administration, statistics, and EXPLAIN;
- typed equality / set / numeric-range queries with stable cursor pagination and resource limits;
- CSV, JSONL, and streaming JSON-array ingestion with rejects, progress, disk preflight, and resumable preparation;
- persistent workload telemetry with P50/P95/P99 and workload-based accelerator recommendations;
- an authenticated role-based HTTP service with rate/concurrency/body/resource limits, audit logging, health/readiness, and Prometheus-style metrics;
- LHR Studio: an API-backed React/TypeScript/TanStack control plane compiled to static assets and served by Rust;
- a stateless MCP control plane for bounded lead queries, diagnostics, query benchmarking, mutations and role-gated administration;
- a single-process Docker appliance with amd64/arm64 image publication;
- an explicit **LHR/1** dataset compatibility contract.

## Correctness rule

**Exactness is non-negotiable.**

Routing may over-select, but it may never exclude a true match. Exact row indexes can prove a result without canonical verification; otherwise surviving candidates are checked against canonical/versioned data.

Updates, deletes, compaction, index recommendations, representation changes, HTTP requests and MCP tool calls preserve the same rule. Performance structures may alter the amount of work, never which logical rows are correct.

## Current benchmark snapshot

These are CI architecture-validation results, not universal production guarantees.

| workload | rows | index amplification | median query | p95 | peak memory |
|---|---:|---:|---:|---:|---:|
| Hybrid-7 low-cardinality synthetic | 1M | ~1.447x | ~0.16-0.18 ms | ~0.32-0.34 ms | ~22 MB |
| Hybrid-7 low-cardinality synthetic | 10M | ~1.445x | ~1.65-1.68 ms | ~3.14-3.17 ms | ~130 MB |
| Mixed-cardinality adaptive | 1M | ~0.702x | ~0.0058 ms | ~0.203 ms | ~45 MB |
| Mixed-cardinality adaptive | 10M | ~0.500x | ~0.0266 ms | ~2.32 ms | ~279 MB |
| Lead-like 12-column workload | 5M | ~1.653x | ~0.042 ms | ~0.86 ms | ~303 MB |
| Lead-like 12-column workload | 10M | ~1.541x | ~0.062 ms | ~2.05 ms | ~522 MB |

A difficult mixed-cardinality query returning roughly **2.5 million rows** fell from about **7.25 ms** to **2.22 ms** after the measured storage/speed tradeoff justified bit-slicing the cardinality-64 field.

The release suite is also exercised under a **1 GiB virtual-memory ceiling**, and the 10M stress jobs carry an explicit sub-1-GiB RSS gate. See [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) for methodology, historical A/Bs, and caveats.

## Why the architecture changed over time

The current design comes from measured failures rather than a one-shot design:

- materializing every hierarchy was fast but caused row-reference storage explosion;
- sparse pair graphs saved bytes but were not a complete exact strategy;
- an exact singleton backbone restored deterministic completeness;
- base-relative block bit-packing worsened both storage and latency and was rejected;
- consecutive-gap packing fixed storage but full posting decompression remained expensive;
- direct compressed intersection reduced decode cost;
- explicit block fences did not justify their metadata cost;
- topology-aware benchmarks exposed that huge low-cardinality singleton composition, not compression itself, was the main remaining tail;
- bit-sliced exact indexes removed that bottleneck;
- flat postings handled the opposite sparse/high-cardinality regime;
- the first CRUD implementation rebuilt a full generation because correctness/stable row-ID semantics mattered more than premature write optimization;
- once those semantics were proven, mutations moved to immutable delta layers + visibility maps to remove routine full-rebuild write amplification;
- compaction then became a separate streaming maintenance operation;
- set/range predicates were added through deterministic exact fallback first rather than inventing an unsafe accelerator;
- workload telemetry/recommendations were added only as optimization inputs, never correctness dependencies;
- the network service deliberately keeps arbitrary filesystem backup/restore paths local to reduce remote administrative capability;
- the MCP control plane follows the same rule: AI clients can operate/query the database, but bulk filesystem import and path-based backup/restore remain local operator actions.

The full retrieval research history is in [`docs/RESEARCH.md`](docs/RESEARCH.md). The operational/product decisions are documented in [`docs/OPERATIONS_RESEARCH.md`](docs/OPERATIONS_RESEARCH.md).

## Operational model

Routine writes create immutable delta layers. Updates reuse the logical row ID, deletes create tombstones, and inserts allocate monotonically increasing IDs. Queries combine base + deltas and suppress stale versions.

`lhr compact` streams the currently visible logical database into a clean base generation while preserving row IDs. Publication is atomic, old generations remain independently valid, and lease-aware vacuum does not remove an active reader's snapshot.

See [`docs/OPERATIONS.md`](docs/OPERATIONS.md).

## Query model

The low-level engine accepts encoded equality predicates. The typed API adds:

- named external values;
- equality;
- set membership;
- inclusive signed/unsigned numeric ranges;
- projection;
- logical-row cursors;
- query timeouts;
- row-examination ceilings.

Equality queries retain the optimized exact-index path. Predicate families without a dedicated exact accelerator use a deterministic versioned-row fallback.

## HTTP service and Studio

`lhr serve` exposes the database through a role-based API. The safe native default binds to loopback only. Remote listeners require authentication and an explicit assertion that TLS/private transport is enforced upstream.

The service includes query/mutation/admin endpoints, body/rate/concurrency/resource limits, audit JSONL, health/readiness endpoints, graceful shutdown, and Prometheus-style metrics. LHR Studio is a static browser application served by the same Rust service and talks to those APIs without a production Node process.

See [`docs/SERVICE.md`](docs/SERVICE.md).

## MCP / AI operator control

The Docker appliance additionally exposes MCP on port `8788`. It uses the same bearer-key role model (`read < write < admin`) and operates against the same snapshot leases, writer lock and immutable generations as the normal service.

The MCP tool surface covers:

- typed lead queries + cursor pagination;
- row/schema/statistics access;
- EXPLAIN, workload and index inspection;
- generations and active leases;
- real process RSS, page faults, disk read/write bytes and filesystem capacity;
- bounded repeated-query benchmarking;
- structural/versioned verification;
- row mutations for write-role clients;
- compaction, vacuum, recovery and exact-index administration for admin-role clients.

This is intended to let an AI client answer normal lead questions and also act as an operator when explicitly granted a stronger credential. Large file ingestion and filesystem backup/restore remain CLI/local by design.

See [`docs/MCP.md`](docs/MCP.md).

## Format compatibility

The current dataset contract is **LHR/1**. Unknown dataset versions are rejected during manifest deserialization. An incompatible future change must use a new format identifier (for example `LHR/2`) and an explicit migration/rebuild path rather than silently changing the meaning of existing bytes.

See [`docs/FORMAT.md`](docs/FORMAT.md).

## Documentation

- [`docs/RESEARCH.md`](docs/RESEARCH.md) — chronological retrieval research: experiments, failures, benchmark-driven decisions.
- [`docs/OPERATIONS_RESEARCH.md`](docs/OPERATIONS_RESEARCH.md) — why the database/product layer evolved from full-generation transactions to deltas, compaction, telemetry, and service hardening.
- [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) — benchmark ledger and historical comparisons.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — retrieval/build architecture.
- [`docs/OPERATIONS.md`](docs/OPERATIONS.md) — implemented database operations and invariants.
- [`docs/SERVICE.md`](docs/SERVICE.md) — HTTP security, API, metrics, and deployment contract.
- [`docs/MCP.md`](docs/MCP.md) — MCP connection, tool, security, diagnostic and AI-operator contract.
- [`docs/FORMAT.md`](docs/FORMAT.md) — LHR/1 on-disk compatibility contract.
- [`docs/STREAMING.md`](docs/STREAMING.md) — historical page-routing/streaming prototype.
- [`docs/RUST_HANDOFF.md`](docs/RUST_HANDOFF.md) — historical Python-to-Rust transition contract.

## Repository layout

- `rust/` — current engine, database product layer, CLI/service/MCP, tests, and scale benchmarks
- `studio/` — React/TypeScript/TanStack browser control plane compiled into the Docker appliance
- `docker/` — one-process appliance entrypoint
- `python/lhr/` — historical/reference research implementation
- `benchmarks/` — benchmark material
- `tests/` — Python/reference correctness tests
- `docs/` — research, architecture, format, operations, service, MCP, and benchmark history

## Core design principles

- Exact deterministic results; no false negatives.
- Push complexity toward ingestion/building rather than repeated retrieval.
- Keep canonical data and large structures disk-backed/mmap-friendly.
- Optimize data/work touched per query, not only theoretical operation counts.
- Use overlapping access paths only when measured storage cost is justified.
- Treat accelerators as performance aids, never correctness dependencies.
- Choose representation from cardinality/density/measured cost, not column semantics.
- Keep logical row identity independent of physical placement.
- Prefer immutable publication + recovery over in-place mutation.
- Reject unknown incompatible formats rather than guessing.
- Keep UI/API/MCP control planes bounded; never let a dashboard or model request silently scale RAM with database size.

## Validation frontier

The database/product/control-plane surface is substantially implemented. Remaining work is primarily validation and optional expansion rather than a missing storage/transaction foundation:

- 25M/50M/70M+ end-to-end scale runs;
- long-running mixed read/write/compaction workloads;
- real lead-data distributions;
- dedicated exact indexes for additional predicate families if workload measurements justify them;
- optional Parquet/pre-tokenized ingest;
- incremental backup/retention policies;
- optional systemd/native deployment conveniences beyond the Docker appliance;
- eventual explicit migrations when an incompatible successor to LHR/1 is warranted.

CI remains the relative engineering gate: correctness first, release tests under the 1 GiB virtual-memory ceiling, then the 1M/5M/10M benchmark/stress suite.

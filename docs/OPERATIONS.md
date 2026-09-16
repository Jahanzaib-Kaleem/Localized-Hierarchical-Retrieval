# LHR Operational Database Layer

LHR is no longer only a retrieval benchmark. The Rust implementation now includes a generation-based operational database around the exact retrieval engine.

The design goal remains the same: deterministic exactness, bounded-memory construction, mmap-friendly reads, and aggressive reduction of query work. Operational features are built around immutable publication rather than weakening those guarantees.

## Operational invariants

1. **Exactness first.** Indexes and workload recommendations may affect cost, never correctness.
2. **Published generations are immutable.** Writers build new state rather than editing live mmap files.
3. **Publication is atomic.** `CURRENT` changes only after the candidate generation has been verified and sealed.
4. **Logical row identity survives rewrites.** Updates/compaction may change physical placement without changing the row ID returned to clients.
5. **Readers can hold snapshots.** A generation can remain readable while newer generations are published.
6. **Durable files are verifiable.** Integrity ledgers detect missing, extra, truncated, or changed files.
7. **Maintenance is first-class.** Recovery, compaction, vacuum, backup/restore, index administration, statistics, and explainability are product operations.
8. **The network service does not become an arbitrary filesystem API.** Path-sensitive backup/restore remain local administrative commands.

## Catalog and transactions

A catalog contains immutable generations plus one atomic publication pointer:

```text
<catalog>/
  CURRENT
  WRITER.lock
  READERS/
  generations/
    00000000000000000001/
    00000000000000000002/
```

A writer acquires the catalog writer lock, constructs unpublished files, verifies them, seals them, renames the staging directory to its final generation ID, then atomically replaces `CURRENT`.

Failed imports, mutations, rebuilds, or compactions do not replace `CURRENT`.

## Schema and dictionaries

`schema.json` defines named columns, logical types, nullability, normalization, and explicit null literals. Per-column mmap dictionaries map canonical external values to deterministic integer tokens and back.

Supported logical types include text, unsigned integer, signed integer, boolean, and timestamp-like text. The storage engine never guesses a column's semantics; normalization is explicit configuration.

## Ingestion

Two ingestion surfaces exist.

### Strict CSV import

`lhr import csv` provides the original two-pass bounded-memory CSV builder. It externally sorts dictionary values, tokenizes rows in batches, writes canonical segments, and builds exact singleton indexes plus configured accelerators.

### External/reject-aware ingestion

`lhr import external` supports:

- CSV;
- JSON Lines;
- streaming JSON arrays;
- schema validation and canonicalization;
- row-level reject JSONL;
- configurable reject ceilings;
- progress files;
- disk-space preflight;
- persistent prepared spools identified by `resume_id`;
- optional unknown-JSON-field rejection;
- the same bounded-memory exact-index builder used by strict import.

The prepared accepted spool can be reused after an interrupted later build phase when the source fingerprint still matches.

## Stable logical row IDs

Physical rows are an implementation detail. LHR exposes monotonically allocated logical row IDs.

- an update retains the existing logical ID;
- a delete leaves the ID unused/tombstoned rather than reassigning it;
- an insert receives a new ID above the previous maximum;
- compaction preserves surviving logical IDs.

`rowids.bin` stores an explicit strictly increasing mapping when identity addressing is insufficient.

## Delta mutations

`lhr mutate` applies a JSON batch as one transaction.

Rather than rebuilding the whole base for routine writes, mutations create immutable delta layers:

- inserts create new delta rows;
- updates create a newer physical row version under the same logical ID;
- deletes create visibility tombstones;
- `overlay.json` records delta layers;
- `visibility.bin` maps overridden logical IDs to the newest layer or deletion.

The versioned reader queries the base and relevant deltas, suppresses stale versions, and returns one exact visible version per logical row.

## Compaction

`lhr compact` streams the visible logical database into a clean base generation. It:

- resolves newest row versions;
- applies tombstones;
- discards dead physical versions;
- preserves logical row IDs;
- rebuilds dictionaries and the chosen exact accelerators;
- publishes the compacted generation atomically.

The previous generation remains valid until normal generation retention/vacuum removes it.

## Snapshots and concurrency

Read-side snapshot leases pin a generation through an OS-level shared lock under `READERS/`. Publication of a new generation does not change an already-open snapshot.

Vacuum checks reader lease files and protects generations still in active use. Normal reads do not take the global writer lock.

Writes/index changes/compaction/publication remain serialized by the catalog writer lock. Concurrent write requests therefore fail cleanly with a conflict rather than interleaving durable state.

## Vacuum

Generation vacuum always preserves:

- `CURRENT`;
- explicitly protected generation IDs;
- configured newest-generation retention;
- generations with active snapshot leases.

It also removes abandoned staging/work directories after safety checks and reports reclaimed bytes.

## Recovery

`lhr recover` is the startup/disaster-recovery operation for a catalog. Recovery:

- inspects published generations;
- verifies layered generation integrity;
- selects the newest fully valid generation;
- repoints `CURRENT` when the current target is missing/corrupt and a known-good generation exists;
- removes abandoned unpublished work.

It does not reinterpret partially written data as valid.

## Verification and integrity

`lhr verify` checks base and operational structures, including manifest/segment consistency, index files, schema/dictionaries, logical row IDs, overlay metadata, visibility references, and integrity seals.

`lhr seal` writes an SHA-256 + byte-length ledger over stable files. Changed, missing, or unexpected stable files make sealed verification fail.

## Backup and restore

`lhr backup` copies a resolved immutable generation to a new destination and verifies the copy before publishing the destination path.

`lhr restore` verifies a standalone generation backup, installs it as a new immutable catalog generation, then atomically publishes it through `CURRENT`.

Backup/restore paths remain local CLI capabilities rather than HTTP endpoints.

## Index administration

The exact singleton backbone is protected because it provides generic exact completeness. Multi-column accelerators can be managed independently:

```text
lhr indexes list
lhr indexes add <column> <column> [...]
lhr indexes drop <column> <column> [...]
lhr indexes rebuild <column> <column> [...]
```

Index reports expose representation (`bitslice`, `deltapost`, `densepost`, `flatpost`, etc.), columns, exact/page status, and storage cost.

Rebuilding allows adaptive physical representation selection to run again on the current data distribution.

## Statistics and EXPLAIN

`lhr stats` reports schema cardinalities, dictionary sizes, canonical/index storage, and index metadata.

`lhr explain` and `lhr explain-values` expose planner decisions: considered/selected indexes, representation and file, seed/intersection roles, candidate counts/pages, exact coverage, and whether canonical verification is required.

This is diagnostic information only; planner choices are not part of the correctness contract.

## Typed query protocol

The higher-level query API supports:

- equality predicates;
- set membership (`IN`);
- inclusive signed/unsigned numeric ranges;
- selected output columns;
- stable logical-row cursor pagination;
- result limits;
- row-examination ceilings;
- timeouts;
- original-value materialization.

Pure equality queries retain the optimized exact LHR index path. Set/range shapes currently use a deterministic exact versioned-row fallback until dedicated exact accelerator semantics are implemented for those operators.

`lhr query-json` accepts this protocol from a JSON file.

## Workload telemetry

Queries can be appended to a local JSONL telemetry stream. Events include:

- predicate columns/operators;
- elapsed microseconds;
- hits;
- rows examined;
- pages touched;
- hierarchy lookups;
- whether the optimized equality path was used;
- exact indexes selected by the planner.

`lhr workload` aggregates P50/P95/P99 by query shape and index-use frequency. It also proposes multi-column equality accelerators when a frequently observed shape lacks one and incurs enough row work to justify investigation.

Recommendations are optimization hints. Applying or ignoring them cannot alter result correctness.

## HTTP service

`lhr serve` exposes the query and operational API. See [`SERVICE.md`](SERVICE.md) for the full contract.

Implemented service protections include:

- secure loopback-only default;
- refusal of non-loopback cleartext exposure unless the operator explicitly declares trusted TLS/private transport upstream;
- Bearer API keys with read/write/admin roles;
- body-size limit;
- per-key rate limit;
- request concurrency limit;
- server-side query/build resource ceilings;
- structured JSON errors with request IDs;
- mutation/admin audit JSONL;
- health/readiness endpoints;
- Prometheus-style runtime/database metrics;
- graceful shutdown.

## Metrics

The service exports counters/gauges for HTTP requests/errors/active requests, query activity, mutation/compaction/admin operations, auth failures, rate limiting, RSS, page faults, process disk bytes, active snapshots, row count, and database storage categories.

Persistent workload telemetry complements these process metrics with per-query-shape latency and planner information.

## Main CLI surface

```text
lhr status
lhr stats
lhr verify
lhr seal
lhr query
lhr query-values
lhr query-json
lhr explain
lhr explain-values
lhr workload
lhr import csv
lhr import external
lhr mutate
lhr compact
lhr indexes list|add|drop|rebuild
lhr backup
lhr restore
lhr recover
lhr generations list|current|rollback|vacuum
lhr serve
```

## What is intentionally still an engineering/validation frontier

The operational architecture is implemented, but that does not mean every possible database feature or workload has been exhausted. Remaining future work is primarily validation and optional expansion rather than a missing transactional foundation:

- larger 25M/50M/70M+ end-to-end datasets;
- real lead-data distributions and long-running mixed read/write workloads;
- dedicated exact accelerators for additional predicate families if measurements justify them;
- optional Parquet/pre-tokenized import surfaces;
- backup retention/incremental-copy policies;
- packaging/deployment conveniences such as systemd/container examples;
- future storage-format migrations when LHR/1 eventually needs an incompatible successor.

The existing CI continues to run correctness tests, a release test suite under a 1 GiB virtual-memory ceiling, and 1M/5M/10M scale benchmarks after operational changes so product work cannot quietly regress the retrieval engine.

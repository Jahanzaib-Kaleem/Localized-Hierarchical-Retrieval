# LHR Operational / Product Research Record

This document records why the LHR project moved from a fast exact retrieval engine into its current database architecture. It complements [`RESEARCH.md`](RESEARCH.md), which focuses on the retrieval/index experiments and benchmark history.

The same decision rule applies here: establish exact/crash-safe semantics first, measure the cost, then optimize without weakening the invariant.

## 1. Retrieval engine to database product

Once the Rust retrieval engine had a stable exact backbone and repeatable 1M/10M benchmarks, the missing problem was no longer only lookup speed. A usable lead database also needed durable external values, imports, mutations, backups, recovery, maintenance, observability, and eventually a network interface.

We intentionally did **not** begin by bolting a server onto raw mmap files. The operational layers were added in dependency order so later features could rely on already-proven publication and recovery semantics.

## 2. Integrity/status/backup came before mutation

The first operational layer added a stable CLI plus:

- dataset status;
- structural verification;
- SHA-256 integrity seals;
- verified filesystem backup.

Why first: before adding writers, the system needed a way to answer two basic questions deterministically: "is this dataset structurally valid?" and "did the durable bytes change?"

This established a reusable verification boundary for every later generation/import/mutation path.

## 3. Immutable generations instead of in-place writes

The next change introduced a generation catalog and atomic `CURRENT` pointer.

Alternative considered: mutate canonical/index files in place and journal changes. That can reduce write amplification, but dramatically increases the number of crash states the implementation must reason about, especially with mmap readers and multiple physical index representations.

Chosen model:

1. construct unpublished state;
2. fsync/verify/seal it;
3. install it under an immutable generation ID;
4. atomically repoint `CURRENT`.

This made imports, index rebuilds, rollback, recovery, compaction, and snapshots share one publication primitive.

## 4. External schema and dictionaries

The research engine operated on integer tokens. A database product cannot require callers to maintain private token maps, so LHR added persistent per-column dictionaries and a schema defining names, logical types, nullability, and explicit normalization.

Important choice: semantic meaning remains outside the storage engine. "email", "country", or any other label does not receive hidden treatment. If lowercase/trim behavior is wanted, it is declared in the schema.

This keeps the core architecture generic and deterministic while making queries/imports usable with original values.

## 5. Two-pass bounded-memory ingestion

CSV ingestion was implemented as a transaction:

- first pass discovers/canonicalizes dictionary values with bounded external sorting;
- second pass tokenizes rows in batches;
- canonical segments and exact indexes are built in staging;
- only a verified generation is published.

A malformed import therefore leaves the previous `CURRENT` untouched.

This followed the original LHR philosophy: preprocessing may be expensive, but memory must be bounded and query-time structures should be optimized once.

## 6. First CRUD implementation deliberately rebuilt everything

The first insert/update/delete implementation rebuilt a complete new physical generation.

That was knowingly inefficient. It was chosen because it made the semantics easy to prove:

- logical row IDs remain stable across updates;
- deletes leave gaps rather than recycling identity;
- inserts allocate monotonically increasing IDs;
- dictionaries and every exact accelerator are immediately consistent;
- a failed transaction cannot expose half-mutated state.

This stage separated a correctness question from a write-amplification question. We did not optimize writes until stable logical identity and transactional behavior were covered by regression tests.

## 7. Why CRUD moved to immutable delta layers

Full-generation mutation had obvious write amplification as data grew. Once CRUD semantics were locked down, the implementation moved to append-only delta layers.

Current model:

- insert -> new logical ID + new delta row;
- update -> new physical version under the same logical ID;
- delete -> tombstone;
- visibility map -> newest visible layer/deleted state for overridden IDs;
- overlay catalog -> ordered immutable delta layers.

The base remains untouched for routine writes. Versioned queries combine base + deltas and suppress old versions.

This is the important architectural transition: **we optimized write cost without changing externally visible mutation semantics.**

## 8. Compaction became separate from mutation

Once routine writes no longer rewrote the base, dead/stale physical versions needed a reclamation mechanism.

Compaction is intentionally explicit rather than hidden inside every mutation. It streams the currently visible logical database into a clean generation, preserves logical IDs, rebuilds dictionaries/indexes, and publishes atomically.

This gives operators control over when expensive consolidation happens and makes compaction failure equivalent to any other failed unpublished build: the old generation stays current.

## 9. Snapshot leases and vacuum

Immutable generations already gave readers a conceptually stable snapshot, but generation garbage collection introduced a lifecycle race: an old generation must not be removed while a reader still depends on it.

Snapshot leases therefore use per-generation shared OS locks. Lease-aware vacuum probes those locks and preserves active generations.

This avoids a global read lock. Normal queries can coexist while a writer builds/publishes a new generation.

## 10. Recovery follows verification, not heuristics

Recovery scans published generations and chooses a generation only if layered verification succeeds. It may repair a missing/corrupt `CURRENT` pointer by selecting the newest known-good generation and can clean abandoned unpublished work.

It never treats "newest directory" or "largest file" as evidence of validity. The integrity/structural verification work from the first operational stage is reused here.

## 11. Index administration and statistics remain performance-only

Multi-column exact accelerators can be added, dropped, or rebuilt. Single-column exact completeness is protected.

This boundary is deliberate: administration can change disk usage and latency, but dropping an accelerator cannot make a formerly correct query incorrect.

Stats expose cardinality, storage, and index representation. EXPLAIN exposes the planner path, candidate sizes, selected exact indexes, and canonical verification requirement.

## 12. Why typed set/range predicates started with fallback

Equality is the native highly optimized LHR access pattern. The product query protocol later added `IN` and numeric ranges.

Rather than pretending existing equality structures could safely accelerate every new operator, LHR implemented exact versioned-row fallback first. Pure equality continues to use the optimized exact index path; unsupported shapes scan deterministically under explicit row/time limits.

This repeats an earlier research lesson: a slower exact path is preferable to a fast structure with an unclear false-negative contract.

If real workload telemetry later shows range/set queries matter enough, dedicated exact representations can be researched and benchmarked independently.

## 13. Persistent workload telemetry before automatic indexing

The system records query shape, latency, rows examined, page/index work, and planner-selected indexes. Aggregation produces P50/P95/P99 and candidate multi-column equality accelerators.

Why recommendations instead of automatic silent index creation:

- extra indexes consume disk/build time;
- workload may be transient;
- measured benefit depends on selectivity/cardinality;
- correctness does not require the accelerator.

The recommendation layer therefore informs an explicit storage/performance decision instead of silently changing database topology.

## 14. Rich ingestion: rejects, resume, progress, preflight

Production input is imperfect, so ingestion was expanded beyond strict CSV to CSV, JSONL, and streaming JSON arrays with:

- row-level reject records;
- configurable reject ceiling;
- progress state;
- source fingerprinting;
- persistent prepared spool/resume ID;
- disk-space preflight.

The prepared-spool boundary was chosen because dictionary/index construction can be expensive. If preparation already succeeded and the source has not changed, repeating parsing/validation is unnecessary.

## 15. HTTP service was intentionally last

Only after generations, verification, CRUD/deltas, compaction, recovery, typed queries, and resource limits existed did LHR add `lhr serve`.

The service reuses the local database API rather than creating separate storage semantics.

Security decisions:

- loopback is the default;
- non-loopback cleartext bind is refused unless an operator explicitly declares trusted TLS/private transport upstream;
- remote listeners require API keys;
- roles are read/write/admin;
- request size, rate, concurrency, query, and builder resources are bounded;
- mutation/admin actions are audit logged;
- health/readiness and metrics are separate operational surfaces.

Arbitrary backup/restore filesystem paths are **not** exposed over HTTP. Those remain local CLI operations. This reduces what a stolen remote administrative credential can ask the database process to do to the host filesystem.

## 16. Observability is split into two layers

Runtime metrics answer "what is the process doing now?": RSS, page faults, disk bytes, active requests, query count/failures, mutation/compaction counters, active snapshots, and dataset storage.

Persistent telemetry answers "what workload has this database seen?": query shapes, latency percentiles, examined rows, and index usage.

Keeping them separate prevents a process restart from erasing workload history while avoiding high-cardinality per-query labels in the Prometheus endpoint.

## 17. LHR/1 format freeze

Early research documentation correctly described the repository-wide format as unfrozen. Once operational generations/deltas/recovery/service depended on persisted datasets, continuing to accept arbitrary `LHR/*` manifests became too loose.

The current release line therefore declares **LHR/1** explicitly. Manifest deserialization rejects unknown dataset versions. Individual structures retain their own versioned magic markers.

This does not mean LHR will never change. It means an incompatible change must be honest: introduce `LHR/2` (or a new component format), then provide an explicit migration/rebuild path.

## 18. What the CI threshold means

The project does not treat a specific Oracle VPS as a prerequisite for every engineering decision. GitHub CI is the repeatable relative gate:

- exact correctness tests;
- release test suite under a 1 GiB virtual-memory ceiling;
- 1M benchmark variants;
- mixed-cardinality 10M stress;
- lead-like 5M/10M stress;
- explicit sub-1-GiB RSS gate on the 10M Hybrid test.

A real machine is still useful for deployment/cache/block-device characterization, but architectural changes are compared first under the same reproducible CI constraints.

## 19. Current boundary

The major database foundations discussed during development are now represented in code: immutable publication, stable row IDs, delta CRUD, compaction, snapshot-aware GC, recovery, integrity, ingestion, typed queries, explain/stats, workload telemetry, index administration, and the HTTP service.

Future work is mainly further validation or optional surface expansion (larger 25M/50M/70M+ runs, real lead distributions, additional exact predicate indexes, Parquet/pre-tokenized ingestion, incremental backup policies, packaging/deployment recipes), rather than a need to redesign the transaction model again.

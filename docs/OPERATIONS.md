# LHR Operational Database Layer

LHR is moving from an architecture/benchmark project into a full database product. This document tracks the operational surface required around the exact retrieval engine.

The target is not a deliberately minimal database. The goal is a coherent, production-grade system whose storage engine remains deterministic, exact, mmap-friendly, and independent of semantic knowledge about columns.

## Operational principles

1. **Exactness remains the first invariant.** Operational features must never weaken retrieval correctness.
2. **Crash safety beats convenience.** A partially completed mutation must not become the visible dataset generation.
3. **Readers observe stable generations.** Long queries must not see a mixture of old and new files.
4. **Every durable file can be verified.** Corruption should be detectable before incorrect data is returned.
5. **Maintenance is explicit and inspectable.** Compaction, reindexing, vacuuming, backup, restore, and verification are first-class operations.
6. **Observability is part of the product.** Storage, planner choices, query work, and maintenance cost should be measurable.

## Foundation implemented first

The initial operational branch adds the first user-facing `lhr` executable and a reusable Rust operations module.

### `lhr status`

Reads the dataset manifest and reports:

- format
- rows / columns / pages
- segment count
- hierarchy count
- canonical bytes
- routing/index bytes
- total dataset bytes
- whether an integrity seal exists

### `lhr verify`

Checks structural invariants before opening the dataset:

- valid manifest and cardinality shape
- contiguous segment row ranges
- contiguous page addressing
- segment headers, lengths, row counts, and column counts
- existence/validity of routing structures through a full `Engine::open`
- integrity manifest completeness when present
- SHA-256 and byte length for every sealed stable file

A dataset can be structurally valid but unsealed; that is reported as a warning. Once sealed, changed/missing/extra stable files make verification fail.

### `lhr seal`

Computes an integrity ledger over all stable dataset files (excluding temporary build state) and atomically publishes `integrity.json`.

The seal records:

```text
relative path | byte length | SHA-256
```

A seal is intentionally explicit rather than silently refreshed. Any legitimate mutation must finish completely before publishing a new seal.

### `lhr query`

Provides the first stable command-line query surface over encoded values:

```bash
lhr --root /data/my-db query 0=4 3=17 --limit 100
```

It returns matching row IDs plus planner/query statistics in JSON.

### `lhr backup`

Creates a filesystem snapshot at a new destination, verifies the copied dataset, and only then publishes the destination directory. Failed copies remain invisible and are cleaned up.

## Full product workstream

The remaining operational layer is divided into durable generations, mutation/compaction, schema/dictionaries, query/result APIs, observability, and network service concerns.

### 1. Dataset generations and transactions

The current single-manifest layout will evolve toward generation-based publication:

```text
<dataset>/
  CURRENT
  generations/
    000000000001/
      manifest.json
      integrity.json
      canonical/
      routing/
      dictionaries/
    000000000002/
      ...
  temp/
```

A writer constructs a generation in isolation, fsyncs all durable files, seals/verifies it, then atomically moves `CURRENT` to the new generation. Existing readers can continue using the old generation until they close it.

This is the basis for:

- atomic imports
- crash recovery
- online index rebuilds
- online compaction
- snapshots
- rollback
- reader/writer isolation

### 2. Ingestion

Production ingestion should support:

- CSV
- JSON/JSONL
- Parquet when appropriate
- pre-tokenized binary batches
- schema mapping
- configurable null representation
- validation/reject files
- bounded-memory batching
- resumable large imports
- disk-space preflight estimates
- progress reporting

Ingestion must never publish half-built canonical/index state.

### 3. Dictionaries and external values

The current engine operates on deterministic integer tokens. The product layer therefore needs durable dictionaries that map external values to tokens and back.

Requirements:

- per-column dictionary metadata
- deterministic token allocation
- persistent reverse lookup
- append of unseen values
- null handling
- dictionary checksum/version
- optional normalization configured by the user, never inferred semantically by the engine
- ability to return original values in query results

### 4. Inserts

New records should enter append-only delta generations/segments rather than forcing a complete rewrite. Exact indexes for the delta are built separately and queried alongside base indexes.

Compaction later merges base + deltas.

### 5. Updates

Updates should be represented as deterministic new row versions plus a visibility/version map rather than unsafe in-place mutation of mmap files.

The query layer resolves the newest visible version. Compaction materializes a clean base generation later.

### 6. Deletes

Deletes should initially use compact tombstone/visibility structures. Deleted rows disappear immediately from logical results, while physical bytes are reclaimed during compaction/vacuum.

### 7. Compaction and vacuum

Compaction should:

- merge base and delta segments
- apply updates/deletes
- rebuild chosen indexes
- discard dead row versions
- publish a new generation atomically
- preserve the old generation until no active reader requires it

Vacuum removes unreachable generations and abandoned temp files after safety checks.

### 8. Index management

Operational commands should include:

- list indexes
- build index
- drop index
- rebuild index
- explain storage cost
- show representation chosen (`bitslice`, `deltapost`, `densepost`, `flatpost`, etc.)
- analyze workload and recommend candidate accelerators

Recommendations can use measured query frequency/selectivity, but correctness must remain independent of the recommended topology.

### 9. Statistics and planner metadata

Persistent stats should include:

- cardinality
- value frequency/skew summaries
- per-index bytes
- posting density
- query frequencies
- observed candidate sizes
- route/intersection costs
- p50/p95/p99 by query shape

Stats are optimization inputs only. Stale stats may hurt performance but may not change correctness.

### 10. Query API

The encoded-token API should expand into a proper typed query surface with:

- equality predicates
- result limits
- deterministic pagination/cursors
- selected output columns
- row ID retrieval
- original-value retrieval through dictionaries
- explain mode
- query timeout/resource limits
- stable JSON protocol

Range/set predicates can be added only when their exact indexing/fallback behavior is defined.

### 11. Result materialization

Finding matching row IDs is only half of a database product. Result materialization needs efficient column projection from canonical segments without reading unrelated fields.

This may motivate a more columnar canonical layout in a future format generation while preserving stable logical row IDs.

### 12. Concurrency and snapshots

Concurrency model:

- many readers per immutable generation
- one or more writers build unpublished generations/deltas
- publication is atomic
- readers retain their generation handle for snapshot consistency
- old generations are garbage-collected only after readers release them

No reader should need a global database lock for normal queries.

### 13. Crash recovery

On startup the operational layer should:

- read `CURRENT`
- verify the referenced generation
- ignore or clean abandoned temp generations
- detect incomplete publication
- optionally fall back to the last known-good generation
- never guess that partially written bytes are valid

### 14. Backup and restore

The current verified filesystem backup is the first step. Full support should include:

- snapshot by generation ID
- incremental/hard-link/reflink-friendly backups where available
- restore verification
- backup manifest metadata
- retention policies
- point-in-time generation selection

### 15. Resource safety

Before expensive operations LHR should estimate:

- required temporary disk
- resulting canonical/index bytes
- expected memory ceiling
- file descriptor use

Runtime limits should be configurable for builders and queries.

### 16. Observability

Expose metrics for:

- RSS
- page faults
- bytes read/written
- build throughput
- compaction throughput
- query latency histograms
- candidate rows
- pages touched
- hierarchy lookups
- index hit/use frequency
- open readers/generations
- disk usage by category

CLI output should support both human-readable and JSON modes; a network server can export metrics later.

### 17. Maintenance command surface

Planned command families:

```text
lhr status
lhr verify
lhr seal
lhr query
lhr backup
lhr restore
lhr import
lhr append
lhr update
lhr delete
lhr compact
lhr vacuum
lhr index list|build|drop|rebuild
lhr stats
lhr explain
lhr generations
lhr rollback
lhr serve
```

### 18. Network service

Once the local operational API is stable, `lhr serve` can expose it over HTTP. That layer then requires:

- authentication/authorization
- TLS or trusted reverse proxy deployment
- request size limits
- rate/concurrency limits
- cancellation/timeouts
- structured error protocol
- audit logging for mutations
- health/readiness endpoints

The storage engine should remain usable without the server.

## Development order

The intended order is dependency-driven rather than MVP-driven:

1. integrity/status/verify/backup/CLI foundation
2. immutable generation catalog + atomic `CURRENT`
3. dictionary/schema layer
4. production ingestion
5. append-only deltas/inserts
6. tombstones + row versions for deletes/updates
7. compaction/vacuum and generation GC
8. typed query/result materialization + explain
9. persistent statistics/index management
10. concurrency/snapshot hardening
11. restore/rollback and disaster recovery
12. HTTP service, auth, metrics, administrative API

Each stage should retain the same benchmark discipline as the core engine: correctness gates first, then storage/RAM/latency measurements.

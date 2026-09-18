# LHR Architecture

This document describes the architecture currently merged to `main`. Historical experiments, rejected designs, and benchmark-driven pivots live in [`RESEARCH.md`](RESEARCH.md); the first real-data investigation and later pagination/range experiments live in [`REAL_DATA_RESEARCH.md`](REAL_DATA_RESEARCH.md). Operational evolution is recorded in [`OPERATIONS_RESEARCH.md`](OPERATIONS_RESEARCH.md).

## Objective

Localized Hierarchical Retrieval (LHR) is a deterministic exact structured-data engine and operational database designed for unusually constrained hardware.

The central trade is deliberate:

> spend more work during ingestion and carefully chosen extra disk on exact access structures so repeated queries touch as little data and allocate as little memory as practical.

The original target was a small Linux VPS with roughly 1 GiB of physical RAM and disk-backed storage. The architecture therefore optimizes bytes/work touched, bounded construction memory, stable exactness, and storage amplification together rather than optimizing only asymptotic operation counts.

## System layers

The current product has three distinct layers.

1. **Retrieval/storage engine.** Tokenized immutable segments, mixed-radix keys, adaptive exact row indexes, conservative page routing, and the equality planner.
2. **Versioned database layer.** Schemas/dictionaries, stable logical row IDs, immutable generation publication, delta layers, visibility/tombstones, compaction, integrity seals, recovery, backup/restore, and reader leases.
3. **Product/control plane.** Bucket workspaces, typed query API, HTTP service, Studio, workload telemetry, metrics, and MCP.

The project name reflects the page-localized/hierarchical research that produced the engine, but the modern fast path is usually adaptive exact row indexing and intersection. Hierarchical page routing remains an important conservative fallback rather than the only retrieval mechanism.

## Correctness model

Exactness is the non-negotiable invariant.

LHR has three relevant query outcomes:

1. **Exact row proof.** Selected exact row indexes cover every predicate. Their intersection is the exact result set, so canonical row verification is unnecessary.
2. **Candidate-row/page routing.** Indexes reduce the search space but do not prove every predicate. Surviving rows/pages are checked against canonical data.
3. **Deterministic fallback.** Predicate shapes without a suitable accelerator are evaluated against the visible versioned database under explicit resource ceilings.

Accelerators may be absent, dropped, rebuilt, or changed in representation. None of those operations may change the correct logical result.

## Buckets, catalogs, and generations

The appliance can host multiple independent database workspaces called **buckets**.

The reserved `default` bucket is the original catalog root, preserving pre-bucket installations without a rewrite. Named buckets live under `buckets/<id>`. Each ready bucket is an independent LHR catalog with its own generations, indexes, deltas, visibility state, workload telemetry, and recovery history.

A catalog contains immutable published generations and one atomic publication pointer:

```text
<bucket-catalog>/
  CURRENT
  WRITER.lock
  READERS/
  generations/
    00000000000000000001/
    00000000000000000002/
```

Writers build unpublished state while holding the catalog writer lock. Publication verifies and seals the staged generation, renames it to its immutable generation ID, then atomically replaces `CURRENT`. Failed work never becomes visible merely because files were written.

Readers resolve `CURRENT` once and hold a snapshot lease on that generation. Publishing a newer generation does not change an in-flight reader's view.

## Physical LHR/1 dataset

A physical generation is an `LHR/1` dataset. The exact file set varies depending on indexes and whether the generation contains an overlay, but conceptually it contains:

```text
generation/
  manifest.json
  schema.json
  integrity.json
  dictionaries/
  canonical/
  routing/
  rowids.bin          # only when logical IDs are not identity-mapped
  overlay.json        # when immutable delta layers exist
  visibility.bin      # newest-layer/tombstone overrides
  deltas/
```

Canonical segments store deterministic integer tokens. Per-column dictionaries translate between canonical external values and tokens. The engine does not assign hidden meaning to a field: logical type, nullability, normalization, and null literals are explicit schema configuration.

## Keys and exact backbone

Single-column indexes use the token directly as their key.

For a compound index over columns with cardinalities `r0, r1, ...`, LHR deterministically packs the selected token tuple into one mixed-radix integer key. The operation is exact and reversible with respect to the configured cardinalities; it is not a hash and introduces no collision semantics.

Every imported physical layer receives exact singleton coverage for every column. Optional pair/wider indexes are accelerators.

This gives LHR an **exact singleton backbone plus selective multi-column accelerators**. Sparse accelerator topology can therefore affect speed without becoming a correctness requirement.

## Adaptive exact representations

No single posting layout is best for every density/cardinality regime. The exact builder chooses among several representations.

### `bitslice`

For dense low/moderate-cardinality singleton equality predicates.

A keyspace is represented by row-wide bit planes. Equality masks are reconstructed word-at-a-time, and two bit-sliced predicates can be intersected directly as machine-word masks before row IDs are materialized.

### `deltapost`

For compressible sorted postings.

The current `LHRDPB3` layout stores fixed-size blocks as one absolute first row plus bit-packed consecutive gaps. When intersecting against an existing sorted candidate seed, gaps are decoded progressively instead of first materializing the entire posting list.

### `densepost`

For keyspaces compact enough that dense offset addressing is efficient.

The posting body is sorted by key/row and direct offsets identify each key's contiguous row interval. This representation also supports bounded seeking from a physical lower bound for the single-predicate pagination path.

### `flatpost`

For sparse/high-cardinality keyspaces where a dense directory would be wasteful.

It stores sorted `(key,row)` records and binary-searches key bounds.

### `postings`

A general sparse posting representation retained as another exact candidate.

## Adaptive construction

Exact-index construction is bounded-memory:

1. scan immutable canonical segments;
2. emit `(key,row)` records into temporary spools;
3. external-sort using bounded runs;
4. measure the observed key distribution;
5. estimate candidate layouts;
6. measure real `deltapost` bytes because row locality affects compression;
7. allow a limited storage premium for low/moderate-cardinality bit-slices when their expected query work is substantially better;
8. write the chosen representation and publish its metadata.

Representation selection is deterministic and data-driven. It does not depend on a field being called `country`, `industry`, or anything else.

## Equality planner

For encoded equality predicates, the engine:

1. validates columns/tokens and collapses impossible/conflicting predicates;
2. finds every exact row hierarchy whose indexed columns are contained in the query;
3. computes each candidate hierarchy's exact row count for its query key;
4. repeatedly chooses the candidate with the best candidate-count/coverage-gain tradeoff for still-uncovered predicates;
5. sorts the selected covering set by candidate count;
6. if the first two selected indexes are bit-sliced, intersects their equality masks directly;
7. otherwise materializes the smallest selected seed;
8. progressively intersects the remaining selected indexes against the shrinking sorted seed;
9. returns immediately when the selected exact indexes cover all predicates;
10. verifies surviving candidates against canonical rows when exact coverage is incomplete.

The planner can therefore combine overlapping indexes rather than requiring one monolithic compound index for every possible query shape.

## Typed query layer

The product query API sits above the encoded equality engine and preserves stable logical-row semantics across base + delta layers.

### Equality

Equality-only queries retain the exact-index route. Cursor pagination translates `after_row_id` into a lower bound inside each immutable layer instead of replaying an ever-growing prefix.

For one exact predicate backed by `bitslice` or `densepost`, the engine can additionally seek from the lower bound and produce at most the requested page while reading the total hit count directly from the index. Other representations and general multi-index plans still use the established candidate-materialization path.

### Empty-filter browsing

An empty filter list is the exact table-browse path used by Studio/API clients. It advances in stable logical-row order and materializes only the requested page.

### Narrow bounded integer ranges

A request containing exactly one bounded signed/unsigned integer range can avoid the general scan fallback when:

- both bounds exist;
- the interval spans at most 256 integer values;
- the relevant column has exact singleton coverage in the base and every delta layer.

The API expands the interval into disjoint exact equality streams, keeps only one head row per stream, and merges them by stable logical row ID. This adds no new on-disk range format.

### General set/range fallback

Wider ranges, open-ended ranges, mixed set/range predicate shapes, and other unsupported cases use the deterministic visible-row fallback under explicit row-examination and timeout ceilings.

## Stable logical row IDs and deltas

Physical row position is separate from client-visible logical identity.

Routine mutations create immutable delta layers:

- an insert allocates a new monotonically increasing logical row ID;
- an update writes a newer physical version under the same logical row ID;
- a delete writes a tombstone;
- `overlay.json` records immutable delta layers;
- `visibility.bin` maps overridden logical IDs to their newest layer or deletion.

The versioned reader combines the base and deltas and exposes exactly one visible version per logical row.

`lhr compact` streams visible rows in logical-row order into a clean physical base, rebuilds dictionaries/indexes, preserves logical IDs, and publishes the result as another immutable generation.

## Conservative page-routing fallback

Page hierarchies store exact key-to-page membership rather than exact key-to-row membership. Routing is intentionally one-sided:

- a page containing a true match must never be omitted;
- extra candidate pages are allowed.

Applicable page sets are intersected before canonical access. Rows inside surviving pages are then checked exactly.

This preserves the original hierarchical/localized idea while allowing the modern exact-row layer to bypass page reads entirely for fully covered equality queries.

## Memory and I/O model

Large structures are mmap-friendly and builders use bounded batches/external sorting.

The desired query-time behavior is that resident memory depends primarily on touched index/canonical pages and bounded candidate state rather than total database size. Important mechanisms include:

- direct bit-slice mask composition;
- compressed-posting intersection against an existing seed;
- progressive candidate reduction;
- cursor lower-bound seeking;
- bounded page production for single `bitslice`/`densepost` predicates;
- streaming visible-row iteration for compaction and fallback operations.

Mmap virtual-address size is not itself a physical-RAM measurement. Production characterization therefore considers RSS/HWM, page faults, process reads, cache state, and whole-system behavior in addition to latency.

## Durability and recovery

The durable contract is now explicit rather than research-only.

- Dataset format: `LHR/1`; unknown manifest versions are rejected.
- Schema format: `LHR-SCHEMA/1`.
- Published generations are immutable.
- Stable files can be sealed with SHA-256 + byte length in `integrity.json`.
- Recovery verifies published generations and can repoint `CURRENT` to the newest fully valid generation.
- Backup/restore verify the copied/restored physical generation.
- Reader leases prevent vacuum from unlinking an active snapshot.
- Writers are serialized per catalog by `WRITER.lock`.

See [`FORMAT.md`](FORMAT.md) and [`OPERATIONS.md`](OPERATIONS.md) for the durable/operational contract.

## Current measured state

Synthetic CI currently validates exact retrieval at up to 10M rows across low-cardinality, mixed-cardinality, and lead-like distributions. Representative merged results include:

- Hybrid-7 10M: ~1.65-1.68 ms median, ~1.445x index amplification;
- mixed-cardinality adaptive 10M: ~0.027 ms median, ~0.500x index amplification;
- lead-like 12-column 10M: ~0.06 ms median, ~1.54x index amplification.

The first non-synthetic Shopify run used 1,902,012 rows / 16 columns on the ~1 GiB VPS. It confirmed very fast warm exact equality paths but also exposed the narrow-range scan and deep-pagination problems that motivated PRs #20-#22. The preserved baseline predates those fixes and must not be rewritten as post-fix evidence.

See [`BENCHMARKS.md`](BENCHMARKS.md), [`REAL_DATA_RESEARCH.md`](REAL_DATA_RESEARCH.md), and [`../benchmarks/REAL_DATA_SHOPIFY_1_9M_BASELINE.md`](../benchmarks/REAL_DATA_SHOPIFY_1_9M_BASELINE.md).

## Frozen contract vs tunable internals

The project is no longer "unfrozen" in the broad sense used by early research notes.

Frozen/current contracts include:

- deterministic exactness;
- explicit `LHR/1` compatibility checking;
- stable logical row-ID semantics;
- immutable generation publication/recovery rules;
- explicit schema/dictionary canonicalization behavior.

Still tunable without changing correctness include:

- which optional multi-column accelerators exist;
- which compatible exact representation a rebuilt index chooses;
- workload-driven accelerator recommendations;
- page size/build resource settings within the supported format.

## Current limits and research frontier

Important limits should be stated explicitly:

- Current LHR/1 exact row-posting families use local `u32` physical row IDs. One physical layer therefore cannot address more than `u32::MAX` rows. Widening those structures silently would violate the format contract.
- The D0 multi-shard composition prototype described in `REAL_DATA_RESEARCH.md` is **research only and not merged architecture**. A durable design still needs cross-shard snapshot/publication, mutation routing, global-ID allocation, integrity, compaction, and telemetry semantics.
- Bounded single-exact pagination currently specializes `bitslice` and `densepost`. `deltapost`, `flatpost`, generic `postings`, and general multi-index plans can still materialize a full final candidate vector before a small result page is taken.
- Only the narrow single bounded integer-range shape has the zero-new-storage equality-decomposition optimization. Broad/open-ended/mixed ranges still use the deterministic scan fallback.
- 25M/50M/70M+ builds, cold-cache/block-device behavior, long-running mixed write/read/compaction workloads, and post-fix real Shopify reruns remain validation work.

These are engineering/scale frontiers, not missing correctness semantics.

# LHR Architecture

This document describes the current architecture. For the experiments that led here, including rejected designs and benchmark-driven changes, see [`RESEARCH.md`](RESEARCH.md). For measurements, see [`BENCHMARKS.md`](BENCHMARKS.md).

## Objective

Localized Hierarchical Retrieval (LHR) is a deterministic exact-retrieval architecture for very large structured datasets under severe RAM constraints.

The central trade is deliberate:

> spend more work during ingestion, and some additional disk on carefully selected exact/routing structures, so queries touch as little data and allocate as little memory as practical.

The target environment is a small Linux VPS with roughly 1 GB of physical RAM and disk-backed storage.

## Correctness model

Canonical data is authoritative. Every optimization must preserve exactness.

LHR has two classes of acceleration:

1. **Exact row indexes.** If selected exact indexes fully cover all predicates, their intersection proves the result set and canonical verification is unnecessary.
2. **Conservative page routing.** If exact indexes do not fully cover a query, page-level structures may return a superset of candidate pages. Canonical rows in those pages are then checked exactly.

An accelerator may be absent or inefficient. It may never be required for correctness.

## Dataset model

Each dataset is independent.

External values are represented by deterministic integer tokens. The engine itself assigns no semantic importance to a token or column. Rows receive stable row addresses and are stored in canonical segments.

Conceptually:

```text
Dataset
├── manifest.json
├── canonical/
│   └── immutable/mmap-friendly row segments
├── routing/
│   ├── exact row indexes
│   └── conservative page hierarchies
└── temp/
    └── bounded-memory build/sort intermediates
```

## Exact backbone

The production direction is an **exact singleton backbone plus selective multi-column accelerators**.

For each indexed column, a singleton exact hierarchy guarantees that a predicate on that column can participate in deterministic exact composition. Selected pair or wider hierarchies can then accelerate frequent or expensive combinations without becoming a completeness requirement.

This is why sparse graph topology is safe: connectivity affects speed, not whether a true row can be found.

## Adaptive exact representations

No one posting layout is optimal for every cardinality/density regime. LHR currently supports several exact row-index representations.

### `bitslice`

Best for dense low/moderate-cardinality equality predicates.

For a keyspace with `b` bits, the index stores `b` bit-planes across all rows. Equality is produced word-at-a-time. Two bit-sliced predicates can be intersected directly as machine-word masks before row IDs are materialized.

This avoids decoding million-row singleton postings for low-cardinality values.

### `deltapost`

Sorted row IDs are divided into fixed-size blocks. Each block stores its first row plus bit-packed **consecutive gaps**.

The current format is the v3 layout (`LHRDPB3`). Intersections stream through compressed gaps directly against the current sorted seed rather than decoding the whole posting into a temporary vector.

### `densepost`

Uses dense keyspace addressing when the keyspace is compact enough that direct offsets are cheaper than sparse directory metadata.

### `flatpost`

Stores sorted `(key,row)` records with minimal structural overhead. It is useful when theoretical keyspace cardinality is enormous but observed keys are sparse, especially high-cardinality pair accelerators.

### `postings`

General sparse posting representation retained as a fallback candidate.

## Adaptive builder

Exact-index construction is bounded-memory:

1. scan canonical segments;
2. emit `(key,row)` records to temporary spools;
3. external-sort and deduplicate with bounded runs;
4. measure/estimate candidate representation sizes;
5. choose an appropriate exact layout;
6. publish hierarchy metadata into the manifest.

For delta compression, actual encoded size is measured because locality materially affects compression ratio.

For low/moderate-cardinality singleton fields, representation choice also includes a query-work budget: a slightly larger bit-slice may be selected when it is expected to avoid far more posting decode/intersection work.

## Query planner

Given predicates `(column,value)`:

1. Reject an invalid column/value immediately.
2. Deduplicate repeated predicates; conflicting values for the same column yield an empty result.
3. Find every exact row hierarchy whose columns are contained in the query.
4. Compute each hierarchy's exact row count for the query key.
5. Greedily choose hierarchies by coverage gained relative to candidate count until no more query columns can be covered.
6. Sort selected hierarchies by row count so composition starts from a small candidate set.
7. If the first two are bit-sliced, intersect their equality masks directly.
8. Intersect remaining selected indexes against the shrinking sorted row seed.
9. If the selected exact indexes cover every query predicate, the surviving row set is exact and the engine returns it without canonical I/O.
10. Otherwise, verify the exact predicates against those candidate rows.

If there is no useful exact row plan, the engine falls back to page routing.

## Page-routing fallback

Page hierarchies store conservative page membership rather than exact row membership. Multiple applicable page sets can be intersected before canonical data is touched.

The invariant is one-sided:

- false-positive candidate pages are allowed;
- false-negative candidate pages are not.

After routing, canonical rows are checked exactly.

## Why overlapping hierarchies are allowed

A single physical row order cannot make all useful query combinations contiguous. LHR therefore treats hierarchies as independent overlapping access paths.

The optimization problem is not "find the one correct tree." It is:

> choose a small portfolio of exact/routing structures whose storage cost is justified by the query work they eliminate.

Workload-aware pair selection can improve this further, but the storage format and correctness rules do not depend on learned or probabilistic behavior.

## Storage model

Canonical records are stored once.

Indexes store compact integer metadata, compressed row addresses, bit-planes, or page addresses depending on the representation.

The principal storage metric is:

```text
index amplification = index bytes / canonical bytes
```

Total dataset size relative to canonical is therefore:

```text
total ratio = 1 + index amplification
```

The project optimizes both storage amplification and query work; minimizing one while ignoring the other has repeatedly produced bad designs.

## Memory model

Canonical segments and indexes are mmap-friendly. Query-time resident memory should depend mainly on touched pages, candidate sets, and OS page-cache behavior rather than total database size.

Large posting materialization is specifically avoided where possible:

- compressed postings intersect directly against an existing seed;
- low-cardinality bit-slices intersect word-at-a-time;
- sorted candidate lists are progressively reduced.

## Current measured state

The current merged implementation has demonstrated, in CI synthetic tests:

- exact Hybrid7 retrieval at 10M rows around 1.65-1.68 ms median with ~1.445x index amplification;
- adaptive mixed-cardinality retrieval at 10M rows around 0.027 ms median with ~0.500x index amplification;
- lead-like 12-column retrieval at 10M rows around 0.06 ms median, with broad low-cardinality cases around ~1.9 ms and ~1.54x index amplification.

These are architecture-validation measurements, not production guarantees. See `BENCHMARKS.md` for methodology and caveats.

## Still intentionally unfrozen

The following remain research/production-hardening areas:

- final dictionary format and external-value storage;
- update/compaction policy;
- checksums and crash recovery;
- format compatibility guarantees;
- workload-driven accelerator selection;
- concurrency;
- cold-cache I/O behavior;
- exact production behavior at 25M/50M/70M+ rows on the target VPS.

The exactness invariant is frozen. Representation and topology remain tunable.

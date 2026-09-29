# Hybrid Parquet Storage

Status: runtime implementation branch for `feature/hybrid-parquet-storage`.

Hybrid storage reduces canonical storage amplification for wide datasets without putting Parquet on LHR's exact equality filtering path.

## Non-negotiable performance contract

For a fully covered equality query the route remains:

```text
external values
    -> dictionaries / token lookup
    -> LHR exact row indexes
    -> row-id intersection
    -> exact physical/logical row IDs
```

Parquet is not consulted to prove the result. It is used only after row IDs are known when selected output columns are cold, or on an existing canonical verification/fallback route.

The regression test `rust/tests/hybrid_storage.rs` enforces this for a predicate on a Parquet-backed column by requiring `rows_checked == 0` and `pages_touched == 0`.

## Physical model

A hybrid generation is still an `LHR/1` logical dataset:

```text
generation/
  manifest.json
  schema.json
  storage.json
  dictionaries/
  routing/
  canonical/
    segment-000000.lhr
    segment-000000.parquet
    segment-000001.lhr
    segment-000001.parquet
    ...
  rowids.bin          # when required
  overlay.json        # when required
  visibility.bin      # when required
  deltas/
```

`storage.json` uses `LHR-STORAGE/1` and partitions every logical column exactly once into:

- `hot_columns`: native fixed-width mmap token storage;
- `cold_columns`: Parquet-backed token storage.

Existing LHR/1 generations without `storage.json` are interpreted exactly as before with every column hot, so no existing dataset needs migration.

## Hybrid segment format

Native segments retain `LHRSEG01` unchanged. Hybrid segments use `LHRHYB01`.

A hybrid `.lhr` segment records the full logical column count, the hot/cold logical column partition, row-group size, and the name of its immutable Parquet sidecar. Its payload contains only hot fixed-width tokens.

`Segment::open` hides the physical split from the engine:

- hot values remain direct mmap arithmetic;
- cold values are materialized from the matching Parquet sidecar;
- `Segment::cols()` still reports the full logical column count;
- callers such as `Engine`, exact-index construction, verification, and fallback scans continue to use the existing logical segment API.

This is intentional: the equality planner and exact-index code are not rewritten around Parquet.

## Parquet representation

Cold payloads store LHR dictionary token IDs, not duplicated external strings. Columns are required Parquet `INT32` values with unsigned 32-bit logical semantics; the raw bit pattern is the LHR `u32` token.

The implementation uses Parquet's low-level typed column API rather than Arrow. LHR only requires integer token columns, and avoiding the Arrow bridge materially reduces build/runtime footprint under the project's constrained-hardware target. Snappy is the initial codec because decode speed is prioritized over maximum compression.

Cold reads are localized by row group. Requested rows are grouped by row group, requested columns are decoded once for that group, and each hybrid `Segment` caches the most recently used cold row group. Hot reads do not acquire the cold cache lock.

## Build path

`build_hybrid_u32_batches` accepts the same complete logical token stream as the native builder plus a sorted set of cold columns and a row-group size.

The builder first sees the complete logical token rows, so page-routing metadata retains existing semantics. Each emitted segment is then split physically:

```text
complete token rows
    -> hot columns  -> segment-NNNNNN.lhr
    -> cold columns -> segment-NNNNNN.parquet
```

Exact singleton and optional wider indexes are built afterward through the unchanged `Segment` interface. Cold columns therefore receive the same exact index coverage as hot columns.

## Query behavior

### Fully indexed equality

No canonical verification and no Parquet filtering:

```text
predicate tokens -> exact LHR indexes -> row IDs
```

Only requested cold result values require Parquet materialization.

### Candidate-first mixed predicates

Equality indexes remain the driver. Residual predicates on cold columns may require Parquet reads for the bounded candidate stream, but an available exact equality seed must not turn into a whole-dataset Parquet scan.

### General fallback

An unaccelerated cold predicate can require Parquet canonical reads and will be slower than fixed-width mmap scanning. This remains a correctness path under the existing timeout / rows-examined controls, not the preferred query plan.

### Empty-filter browsing

Hot-only browsing remains native. Cold projections should be consumed in batches/row groups, not by independently opening Parquet per returned cell.

## Durability

Hybrid Parquet sidecars are generation-owned stable files. Because they live inside the generation tree, existing recursive integrity sealing, backup/restore, recovery and immutable-generation lifecycle include them automatically.

`storage.json` validates that hot/cold columns partition the logical schema exactly once and that declared Parquet payload ranges cover the physical layer contiguously.

Arbitrary mutable external Parquet paths are not part of the durable format. Source Parquet can be read during ingestion, but published generations own their canonical payloads.

## Import direction

The intended Apollo-scale flow is:

```text
source Parquet
    -> dictionary/cardinality pass
    -> token batches
    -> hybrid LHR builder
       -> hot mmap tokens
       -> cold owned Parquet tokens
    -> exact LHR indexes
    -> verification + integrity seal
    -> atomic publication
```

The ~145 GB CSV representation should never need to exist just because the source arrives as ~25 GB of Parquet.

## Acceptance gates

Before enabling hybrid storage for a production bucket, benchmark native and hybrid representations on the same real shard and report at least:

- final bytes by hot canonical / cold Parquet / dictionaries / indexes / metadata;
- peak staging bytes during ingestion;
- warm and cold exact equality latency;
- equality pagination latency;
- materialization for 1, 10, 100, 1,000 and 10,000 rows;
- hot-only vs cold projection;
- candidate-first mixed predicates;
- fallback scan cost;
- RSS/HWM, major faults, process reads/writes;
- exact result equivalence.

### Hard acceptance rule

The fully indexed equality row-ID path may not regress because of Parquet. Any cost introduced by cold result materialization must remain isolated to materialization and be measured separately.

## Remaining production enablement

The hybrid segment backend and token builder are implemented. Remaining work before using this for the Apollo bucket is:

1. direct source-Parquet ingestion so the 145 GB CSV intermediate is never created;
2. a user-facing import/profile entry point for choosing hot/cold columns;
3. compaction behavior that preserves or deliberately reselects the hybrid profile instead of silently expanding the dataset back to all-native storage;
4. representative Apollo-shard storage and latency benchmarks to choose the production hot/cold column set and row-group size.

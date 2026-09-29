# Hybrid Parquet Storage

Status: implemented on `feature/hybrid-parquet-storage`.

This design reduces canonical storage amplification for large lead datasets without putting Parquet on LHR's exact filtering path.

## Non-negotiable performance contract

Hybrid storage does not put Parquet on the exact-index filtering path.

For a fully covered equality query the route remains:

```text
external values
    -> dictionaries / token lookup
    -> LHR exact row indexes
    -> row-id intersection
    -> exact physical/logical row IDs
```

Only materialization of requested cold values consults the Parquet payload. Equality planning, posting representations, bit-slice intersections, row-count lookup, and bounded row-ID pagination remain LHR-native.

A regression test enforces that an exact equality predicate on a cold Parquet-backed column returns with `rows_checked == 0` and `pages_touched == 0`.

## Physical model

A hybrid generation contains:

```text
generation/
  manifest.json
  schema.json
  integrity.json
  storage.json
  dictionaries/
  routing/
  canonical/
    segment-000000.lhr
    segment-000000.parquet
    ...
  rowids.bin          # when required
  overlay.json        # when required
  visibility.bin      # when required
  deltas/
```

`storage.json` uses `LHR-STORAGE/1` and partitions every logical column exactly once into:

- `hot_columns`: fixed-width mmap-native LHR token storage;
- `cold_columns`: Snappy-compressed Parquet token storage.

Existing LHR/1 generations without `storage.json` remain legacy native storage with every column hot. They require no migration.

## Canonical representation

Both hot and cold stores contain the same LHR dictionary token IDs. Parquet does not introduce an independent value encoding. Dictionaries remain the single mapping between external values and internal tokens, and exact indexes continue to use those same tokens.

Hybrid native segments use the `LHRHYB01` format. They expose the same logical `Segment` API as legacy segments, so the planner, query engine, verification, index construction, backup, and recovery code do not need a separate query model.

## Direct source-Parquet ingestion

Existing Parquet shards can be imported directly with `lhr-parquet-import`. The importer performs two bounded passes over the source shards:

```text
Pass 1: source Parquet -> canonical values -> external-sort dictionary runs -> LHR dictionaries
Pass 2: source Parquet -> dictionary tokens -> hot native segments + cold Parquet sidecars
                                      -> exact LHR indexes -> verify -> publish
```

No decoded CSV staging copy is created. Source Parquet files are opened read-only and remain untouched at their original paths.

The importer accepts a saved hot/cold profile and additional multi-column exact accelerators. It prints the final storage report and verification result after publication.

See `docs/PARQUET_IMPORT.md`.

## Row groups and materialization

Cold payload row-group size is configurable; the default is 65,536 rows. Requested cold rows are grouped by row group, and a segment caches the most recently decoded cold row group. Exact filtering still happens before any cold payload access.

## Compaction

Compaction no longer creates a full temporary CSV. It performs two bounded passes over visible logical rows:

1. rebuild dictionaries;
2. stream dictionary tokens into a new canonical generation.

Native generations remain native. Hybrid generations preserve their cold-column profile and Parquet row-group size. Columns introduced by schema evolution default to hot storage until a future explicit profile change.

Logical row IDs are preserved through `rowids.bin`.

## Durability

LHR-owned Parquet sidecars are immutable generation files. They are included in integrity sealing, verification, backup/restore, recovery, snapshot/lease lifecycle, and vacuum behavior through the existing recursive stable-file handling.

Arbitrary mutable external Parquet paths are not used as live canonical storage. Source Parquet is imported into generation-owned token sidecars so a published generation is self-contained and durable.

## Compatibility and resource constraints

The implementation uses the low-level Parquet INT32 API rather than Arrow. This keeps the build/runtime footprint compatible with LHR's constrained-hardware goals while preserving full `u32` token bit patterns.

Parquet 60's MSRV is Rust 1.88; the appliance builder uses Rust 1.90.

The existing LHR release suite continues to run under the 1 GiB virtual-memory ceiling, and the unchanged mixed-cardinality, lead-like, Hybrid-7, and topology benchmark gates remain in CI.

## Production acceptance

Before importing a very large real dataset, choose a profile based on which columns must have cheap arbitrary materialization. Exact indexed filtering is independent of hot/cold placement, but frequently returned or residual-scanned columns should remain hot.

The direct importer reports the actual published `canonical_bytes`, `routing_bytes`, and `total_bytes`. This lets the real dataset's storage footprint be measured immediately after import without constructing a CSV intermediate.

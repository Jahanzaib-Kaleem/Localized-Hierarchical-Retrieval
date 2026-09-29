# Hybrid Parquet Storage

Status: implementation branch design contract for `feature/hybrid-parquet-storage`.

This document defines the storage direction for large lead datasets where the current all-native LHR canonical row store can exceed available disk. The goal is to reduce canonical storage amplification without weakening LHR's exact retrieval path.

## Non-negotiable performance contract

Hybrid storage must not put Parquet on the exact-index filtering path.

For a fully covered equality query the route remains:

```text
external values
    -> dictionaries / token lookup
    -> LHR exact row indexes
    -> row-id intersection
    -> exact physical/logical row IDs
```

Only materialization of requested values may consult the Parquet payload. The equality planner, posting representations, bit-slice intersections, row-count lookup, and bounded row-ID pagination remain LHR-native.

A hybrid implementation is not acceptable if an exact equality query must decode Parquet pages merely to determine whether a row matches.

## Why hybrid instead of replacing LHR canonical storage wholesale

The existing native canonical segments are fixed-width and mmap-friendly. They are excellent for arbitrary row/token access and deterministic fallback scans, but are not optimized for compression of wide, repetitive lead datasets.

Parquet is designed for compact columnar storage. It is therefore useful for payload materialization, but random single-cell lookup and whole-row reconstruction can require page decoding. Replacing every canonical access with Parquet would trade away an important LHR property.

Hybrid storage keeps the parts that make queries fast and applies Parquet only where storage density matters more.

## Physical model

A hybrid generation keeps the existing LHR structures and adds an explicit storage contract:

```text
generation/
  manifest.json
  schema.json
  integrity.json
  storage.json
  dictionaries/
  routing/
  canonical/          # hot LHR token columns only
  payload/
    cold-000000.parquet
    cold-000001.parquet
    ...
  rowids.bin          # when required
  overlay.json        # when required
  visibility.bin      # when required
  deltas/
```

`storage.json` uses `LHR-STORAGE/1` and partitions every logical column exactly once into:

- `hot_columns`: mmap-friendly native LHR token storage;
- `cold_columns`: Parquet-backed payload storage.

Existing LHR/1 generations without `storage.json` are interpreted as legacy native storage with every column hot. They require no migration and must retain identical behavior.

## What stays hot

Hot columns are chosen for latency, not merely cardinality. Typical candidates are columns that are frequently returned, used in residual verification, browsed interactively, or otherwise need cheap arbitrary row access.

For a lead dataset these may include identifiers and commonly returned fields such as email, person/company names, title, domain, location, seniority, department, industry, and frequently used numeric dimensions.

This is a workload decision. The storage engine must not hard-code business field names.

## What can be cold

Cold columns are still part of the logical dataset, but their physical values are materialized from Parquet. Large text, descriptions, secondary URLs, sparse metadata, long keyword arrays serialized as text, and rarely returned fields are natural candidates.

A cold column may still have an LHR exact index. Filtering and result proof use that index; Parquet is only required when the value itself must be returned or canonical verification is unavoidable.

## Critical query invariants

### Fully indexed equality

No canonical verification. Parquet must not be opened for filtering.

```text
predicate tokens -> exact indexes -> row IDs
```

After row IDs are known, only selected output columns are materialized. Hot selected columns come from native canonical storage and cold selected columns come from Parquet.

### Candidate-first mixed predicates

Equality indexes remain the driver. Residual predicates on hot columns retain the current cheap verification path.

Residual predicates on cold columns may require Parquet reads for the bounded candidate stream. They must never force a whole-dataset Parquet scan when an exact equality seed is available.

### General fallback

An unaccelerated predicate over a cold-only column can require Parquet scanning. This is expected to be slower than the current fixed-width native fallback and must remain protected by the existing timeout / rows-examined controls.

The system should prefer retaining frequently scanned residual/filter columns as hot, or adding exact accelerators, rather than pretending Parquet has equivalent random-access behavior.

### Empty-filter table browsing

Browse performance depends on selected columns. Hot-only browsing should retain the native path. Cold projections should be read in row batches rather than issuing one Parquet reader per row.

## Parquet payload representation

The first implementation should store canonical LHR token IDs in Parquet rather than duplicating external strings independently of LHR dictionaries.

Reasons:

1. dictionaries remain the single canonical mapping between external values and internal tokens;
2. exact indexes continue to use the same tokens;
3. hot and cold physical representations can be compared without changing logical semantics;
4. fallback verification can compare integer tokens instead of repeatedly canonicalizing strings;
5. cold data benefits from Parquet encoding/compression of repeated integer token streams.

The logical API still returns external values by decoding tokens through the existing dictionaries.

## Row identity

Physical row order remains stable inside each immutable layer. A Parquet payload file declares a contiguous `[row_start, row_start + rows)` physical range. `storage.json` requires payload ranges to be contiguous and to cover the full physical layer.

Logical row IDs remain governed by the existing row-ID map, overlays, deltas, visibility rules, and snapshot semantics. Parquet must not invent a second identity system.

## Row groups and batching

The initial target row-group size is 65,536 rows. This is deliberately a tuning parameter in `storage.json`, not a frozen constant.

Materialization should group requested physical row IDs by payload file and row group, project only requested cold columns, decode a row group once, and satisfy all requested rows from that batch. This avoids the pathological design of opening/decompressing Parquet independently for every returned row.

## Durability

LHR-owned Parquet payload files are immutable generation files. They must be included in integrity sealing, verification, backup/restore, recovery, and reader-lease lifecycle just like current canonical files.

Arbitrary mutable external Parquet paths are not part of the durable design. An import may read external Parquet files, but a published hybrid generation must own or immutably adopt the payload bytes covered by its integrity manifest.

## Import direction

Native Parquet ingestion and hybrid storage are separate concerns but should be implemented together for large datasets:

```text
source Parquet
    -> schema/token dictionary pass
    -> exact-index construction
    -> hot token columns -> native LHR canonical segments
    -> cold token columns -> LHR-owned Parquet payloads
    -> verification / integrity seal
    -> atomic publication
```

A 145 GB CSV should never need to be materialized merely because the source data arrived as ~25 GB of Parquet.

## Storage and speed acceptance gates

Before hybrid storage can replace native storage for a production bucket, benchmark both representations on the same real shard and report at least:

- final generation bytes by native canonical / Parquet payload / dictionaries / indexes / metadata;
- peak temporary bytes during ingestion;
- warm and cold exact equality latency;
- equality pagination latency;
- result materialization for 1, 10, 100, 1,000 and 10,000 rows;
- hot-only projection vs cold projection;
- candidate-first mixed predicates;
- general fallback scan cost;
- RSS/HWM, major faults, process reads and writes;
- exact result equivalence.

### Hard acceptance rule

The hybrid design must not regress the fully indexed equality row-ID path. Any materialization regression must be isolated to output decoding and quantified separately.

For the Apollo-scale migration, do not publish a hybrid generation until its final sealed size plus existing source copies fits the actual VPS disk with operational headroom for staging, append, compaction, backup and recovery.

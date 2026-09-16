# LHR On-Disk Format (Research / Not Frozen)

This document describes the current format concepts. Individual Rust index types already have concrete binary layouts, but repository-wide compatibility is **not yet frozen**. The implementation is the source of truth for byte-level details until a stable format version is declared.

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the current query/build design and [`RESEARCH.md`](RESEARCH.md) for why the representations evolved.

## Requirements

- readable without loading the full dataset into RAM;
- mmap-friendly;
- deterministic;
- compact integer representations;
- explicit format/version markers per structure;
- bounded-memory build path;
- representation choice can evolve independently of query semantics;
- exactness must survive missing/unused accelerators.

## Dataset layout

```text
<dataset>/
  manifest.json
  dictionaries/       # external value <-> deterministic token mapping (still evolving)
  canonical/          # authoritative tokenized row segments
  routing/            # exact row indexes and conservative page hierarchies
  temp/               # build-time spools/runs
```

`manifest.json` records format/version information, row count, column count, cardinalities, page sizing, canonical segments, and hierarchy metadata.

## Canonical data

Canonical rows are authoritative and stored once. Current Rust segments are mmap-friendly and preserve stable row addressing.

Indexes may prove a fully covered exact query without rereading canonical rows. If they do not fully cover a query, canonical data remains the final verifier.

## Keys

Multi-column hierarchy keys use deterministic mixed-radix integer encoding. The engine does not require semantic meaning for any column/value.

A hierarchy's keyspace is the product of its component cardinalities when that product fits the supported integer range.

## Exact row-index families

The manifest records a hierarchy `kind`, allowing different physical representations behind the same exact logical operation.

### `deltapost`

Current packed format magic: `LHRDPB3\0`.

Conceptually:

```text
header
sorted key directory: (key, body_offset, row_count)
body:
  block base row
  block count
  gap bit width
  bit-packed consecutive row gaps
```

Blocks currently contain up to 128 rows. Query-time intersection can stream compressed gaps directly against an existing sorted seed.

### `bitslice`

Current bit-slice format magic: `LHRBSL01`.

Conceptually:

```text
header
per-value exact counts
bit planes over row addresses
```

For a keyspace requiring `b` bits, the index stores `b` row-wide bit planes. Equality for one value is reconstructed word-at-a-time by ANDing the required planes/complements. This is particularly effective for dense low/moderate-cardinality singleton indexes.

### `densepost`

Uses dense keyspace addressing plus row storage. It is useful when the theoretical keyspace is compact enough that a direct offset table is cheaper than sparse key metadata.

### `flatpost`

Stores sorted exact `(key,row)` data with minimal structural overhead. It is useful when the keyspace is enormous but observed entries are sparse, such as high-cardinality pair accelerators.

### `postings`

General sparse exact-posting fallback.

## Conservative page hierarchies

Page-level `sparse` and `bitmap` structures may be used when row-level exact indexes do not fully cover a query.

Their correctness contract is conservative:

```text
true matching page  => must be returned
returned page       => may or may not contain a final matching row
```

Canonical verification removes false-positive pages/rows.

## Representation selection

The exact builder may generate/estimate several candidate layouts for the same logical hierarchy.

Selection considers:

- keyspace cardinality;
- observed unique keys;
- row count;
- real delta-compressed file size;
- estimated dense/flat/sparse sizes;
- a bounded query-work allowance for bit-slices.

This is intentionally adaptive. A representation that is a few bytes larger may be preferable if it eliminates a much larger decode/intersection cost.

## Integer widths

Widths are selected from actual requirements rather than defaulting everything to 64 bits. Current row-posting formats use `u32` row IDs and therefore require fewer than `2^32` rows per dataset format generation.

Wider row addressing can be introduced in a future format version without changing the logical query model.

## Crash safety and compatibility

Manifest publication uses replace-style writes in the current builder, but checksums, crash recovery, compaction semantics, and long-term binary compatibility are not yet considered frozen production guarantees.

The current research format should be treated as versioned implementation data, not a permanent public storage ABI.

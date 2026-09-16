# LHR On-Disk Format

This document defines the compatibility contract for the current **LHR/1** dataset format.

LHR still has room to evolve internally, but incompatible on-disk changes are no longer allowed to masquerade as the same format. A build that supports `LHR/1` rejects a manifest declaring another dataset format. Any incompatible future change must use a new identifier such as `LHR/2` and provide an explicit migration/rebuild path.

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the query/build design, [`RESEARCH.md`](RESEARCH.md) for the retrieval research history, and [`OPERATIONS.md`](OPERATIONS.md) for transactional generations and maintenance.

## Compatibility rule

The top-level dataset manifest contains:

```json
{"format":"LHR/1"}
```

The Rust `Manifest` deserializer validates this identifier. Prefix matching such as accepting arbitrary `LHR/*` formats is intentionally not part of the compatibility contract.

Within LHR/1, individual binary structures also carry their own magic/version markers. This lets the engine reject a damaged or incompatible component before returning data.

An accelerator representation can be rebuilt without changing logical query semantics. Canonical data, dictionaries, logical row IDs, visibility rules, and manifest compatibility are the durable correctness boundary.

## Generation catalog

A production catalog has this shape:

```text
<catalog>/
  CURRENT
  WRITER.lock
  READERS/
  generations/
    00000000000000000001/
      manifest.json
      integrity.json
      schema.json
      rowids.bin                 # present when logical IDs are non-identity
      overlay.json               # present when delta layers exist
      visibility.bin             # newest-version / tombstone overrides
      canonical/
      routing/
      dictionaries/
      deltas/
        0000000001/
          manifest.json
          schema.json
          rowids.bin
          canonical/
          routing/
          dictionaries/
```

`CURRENT` is the only publication pointer. A writer constructs and verifies an immutable generation before atomically repointing it. Readers can keep an older generation pinned with a snapshot lease while a newer generation is published.

Legacy single-generation roots containing `manifest.json` directly remain readable where supported by the catalog resolver.

## Versioned components

The current implementation uses explicit identifiers including:

| structure | identifier / contract |
|---|---|
| dataset manifest | `LHR/1` |
| schema | `LHR-SCHEMA/1` |
| integrity ledger | `LHR-INTEGRITY/1` |
| logical row-ID map | `LHRRID01` |
| overlay catalog | `LHR-OVERLAY/1` |
| visibility map | `LHRVIS01` |
| delta-compressed postings | `LHRDPB3\0` |
| bit-sliced postings | `LHRBSL01` |

Other exact/page index families have their own validated headers/layouts in the Rust implementation. The implementation remains the byte-level source of truth for those component formats.

## Canonical data

Canonical rows are authoritative and stored once per physical layer. Segments are mmap-friendly and preserve physical row addressing inside that layer.

Logical row IDs are separate from physical row locations. This is important because updates create newer row versions and compaction can rewrite physical placement without changing the logical identity exposed to clients.

A query can avoid rereading canonical rows when exact row indexes prove the complete result. Otherwise canonical data is the final deterministic verifier.

## Dictionaries and schema

External values are mapped to deterministic integer tokens by per-column dictionaries. The schema defines:

- column names;
- logical types;
- nullability;
- explicit normalization;
- configured textual null literals.

The storage engine does not infer semantic meaning from a column. Normalization is configuration, not hidden behavior.

Dictionary/token choices are local to a physical base or delta layer. The versioned query layer resolves external values against each layer independently, which allows new values to appear in later delta layers without rewriting the base dictionary immediately.

## Logical row IDs, deltas, and visibility

Base rows normally begin with identity logical row IDs. Once deletes, updates, inserts, or compaction require it, `rowids.bin` stores a strictly increasing logical-ID map.

Mutations do not edit mmap files in place:

- inserts create new logical IDs in an immutable delta layer;
- updates create a new physical row version carrying the existing logical ID;
- deletes create a tombstone;
- `visibility.bin` records the newest visible layer or deletion for overridden logical IDs;
- `overlay.json` records the immutable delta-layer catalog.

A versioned reader combines base + delta layers and returns exactly one visible version of each logical row.

Compaction streams visible rows into a clean base generation, rebuilds dictionaries/indexes, preserves logical IDs, and removes delta history from the new generation. Older generations remain independently valid until vacuumed.

## Keys

Multi-column hierarchy keys use deterministic mixed-radix integer encoding. A hierarchy keyspace is the product of its component cardinalities when that product fits the supported integer range.

## Exact row-index families

The manifest records a hierarchy `kind`, allowing different physical representations behind the same exact logical operation.

### `deltapost`

Packed consecutive-gap blocks. Current format magic is `LHRDPB3\0`. Query-time intersection can stream compressed row gaps against an existing sorted seed without materializing the whole posting list.

### `bitslice`

Current magic is `LHRBSL01`. It stores row-wide bit planes and reconstructs exact equality masks word-at-a-time. It is effective for dense low/moderate-cardinality columns.

### `densepost`

Dense keyspace addressing plus exact row storage. Useful when direct key offsets cost less than sparse metadata.

### `flatpost`

Sorted exact `(key,row)` records with low structural overhead. Useful for enormous theoretical keyspaces with sparse observed values, including high-cardinality accelerators.

### `postings`

General sparse exact-posting fallback.

## Conservative page hierarchies

Page-level `sparse` and `bitmap` structures can reduce canonical work when exact row indexes do not fully cover a query.

Their contract is conservative:

```text
true matching page  => must be returned
returned page       => may or may not contain a final matching row
```

Canonical verification removes false-positive candidates. A routing structure may over-select but may never create a false negative.

## Adaptive representation selection

The exact builder compares representation cost from the actual dataset. Selection considers keyspace, observed keys, row count, measured/estimated file sizes, density, and bounded query-work tradeoffs.

Representation selection is an optimization decision only. Changing or dropping a multi-column accelerator may change speed/storage, never result correctness.

## Integer widths and format limits

Widths are chosen from actual requirements rather than storing all tokens/addresses as 64-bit values. Current exact row-posting families use `u32` physical row IDs and therefore impose the corresponding per-physical-layer addressing limit.

A future wider addressing scheme is an incompatible storage change if it alters these component layouts and must receive a new component/dataset compatibility treatment rather than silently changing LHR/1.

## Integrity and crash safety

Every published generation can carry `integrity.json`, an SHA-256 + length ledger covering stable files. Publication verifies structure, seals the generation, verifies the sealed result, renames the staged directory into its immutable generation ID, and only then atomically updates `CURRENT`.

Interrupted staging/build work is never treated as published data. Recovery searches published generations for a fully verified candidate and can repoint `CURRENT` to the newest valid generation.

## Upgrade policy

Compatible implementation improvements may continue inside LHR/1 when they do not change the interpretation of already-written durable files.

An incompatible change requires one of:

1. a new dataset/component format identifier plus a reader/migration path;
2. an explicit offline rebuild/export-import into the new format.

LHR must not guess that unknown future bytes are compatible. Exact failure is preferable to silently misreading data.

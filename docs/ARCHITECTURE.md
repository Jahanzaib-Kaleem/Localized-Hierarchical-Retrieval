# LHR Architecture v0

## Objective

LHR is an experiment in deterministic exact retrieval from very large structured datasets under severe RAM constraints.

The central tradeoff is deliberate: writes may perform substantial preprocessing and the database may spend additional disk space on routing structures if that allows reads to touch only a tiny fraction of canonical data.

## Conceptual model

A dataset is independent from every other dataset. Within a dataset, values are dictionary-encoded into compact deterministic identifiers. Rows receive stable identifiers and are stored in canonical form.

Above canonical storage sits a collection of localized hierarchies. A hierarchy is not required to have semantic meaning. It exists because a particular combination or partition of values is useful for reducing search space.

The reader should attempt the cheapest/smallest useful localized structure first and fall upward toward broader structures only when required.

## Query path

1. Encode query values into deterministic IDs.
2. Construct addresses for applicable hierarchy entries.
3. Locate entries through direct addressing or binary search.
4. Estimate/select the entry that yields the smallest candidate region.
5. Recursively localize through child regions when available.
6. Read the surviving canonical region(s).
7. Verify every query predicate exactly.

The final verification step guarantees correctness even when routing metadata is intentionally lossy (for example, presence signatures).

## What experiments have shown so far

### Useful

- Compact integer encoding drastically reduces canonical working size.
- Memory mapping allows the reader to operate without loading the database into RAM.
- Multi-column localized indexes can reduce candidate sets dramatically.
- Binary search over sorted folder directories is cheap.
- Different hierarchy widths (2/3/4 columns) have different storage/selectivity tradeoffs.
- A query planner can choose useful hierarchies without assigning semantic importance to columns.

### Rejected / insufficient by itself

- Materializing every possible multi-column hierarchy with a full uint32 row-ID list causes storage growth roughly proportional to `4 * rows * hierarchies`.
- Merely replacing global row IDs with local uint16 offsets still duplicates one reference per row per hierarchy.
- Fixed-size presence-bitset pages are exact as routing filters but become weak when pages contain most possible values.
- A single physical ordering cannot make every overlapping hierarchy contiguous simultaneously.

## Current direction

The hierarchy should primarily describe **where** matching data can exist rather than enumerate every matching row.

Candidate metadata includes:

- sorted keys
- start offsets
- lengths/ranges
- child addresses
- compact presence signatures
- adaptive region boundaries
- optional sparse row references only at leaves where justified

Conceptually:

```text
Dataset
  Canonical storage
  Dictionary/token tables
  Root directory
    Local region
      Child directory
        Micro-region
          Exact canonical rows
```

This is not necessarily a strict tree. Multiple localized hierarchies may overlap. The query planner can choose whichever available route minimizes expected work.

## Optimization target

At write time, candidate hierarchies can be evaluated approximately by:

`utility = expected_search_space_eliminated / additional_storage_bytes`

The exact scoring function remains experimental and may incorporate observed query distributions later, but the core format must not require learned or probabilistic behavior.

## Target environment

The intended stress target is a Linux VPS with approximately 1 GB physical RAM and disk-backed block storage. Query-time memory should therefore be bounded independently of total dataset size wherever practical.

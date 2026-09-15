# Rust Handoff Contract

The Python implementation is the executable specification for the first production implementation.

## Frozen invariants

1. Retrieval is deterministic and exact.
2. Canonical data is authoritative; routing may only over-select.
3. Dataset size must not determine required resident RAM.
4. Ingestion accepts bounded-size batches.
5. Hierarchy construction uses bounded-memory external runs and merge.
6. Hierarchy leaves identify pages/regions rather than duplicating every row ID.
7. Hierarchy records are ordered by deterministic mixed-radix keys and are binary-searchable.
8. Multiple applicable hierarchies may be intersected before canonical I/O.
9. The reader exact-checks all predicates on candidate pages.
10. Hierarchy selection requires no semantic model or LLM.

## Binary hierarchy record

Little endian, 12 bytes:

- `key`: uint64
- `page`: uint32

Records are globally sorted by `(key,page)` and deduplicated. This is deliberately simple enough for mmap plus binary search in Rust.

## Canonical segmentation

Python v0 uses `.npy` canonical segments for research convenience. Rust should replace this with an LHR-native segment header plus packed column/token payload while preserving page numbering and manifest semantics.

## Rust implementation priorities

- mmap hierarchy directories and canonical segments
- zero-copy binary search over hierarchy keys
- galloping/intersection of sorted candidate page lists
- direct page-to-segment addressing
- bounded-memory external run generation and k-way merge
- compact dictionary encoding
- CRC/checksum and crash-safe manifest publication
- benchmark resident set size, page faults, bytes read, p50/p95 latency, build throughput and index amplification

## Not frozen

Adaptive page sizing, hierarchy utility scoring, compression, update/compaction policy, concurrency and the final native segment encoding remain tunable. These are optimizations around the retrieval invariants rather than changes to the central architecture.

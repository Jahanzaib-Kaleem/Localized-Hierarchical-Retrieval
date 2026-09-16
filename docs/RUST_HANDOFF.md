# Rust Handoff Contract (Historical)

> **Historical document.** This captured the Python-to-Rust transition before the current Rust engine existed. Several implementation details below were subsequently superseded by exact row indexes, adaptive posting representations, bit-slices, flat sparse postings, and the current query planner. See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the current design and [`RESEARCH.md`](RESEARCH.md) for how the transition evolved.

The Python implementation was the executable specification for the first production implementation.

## Frozen invariants

1. Retrieval is deterministic and exact.
2. Canonical data is authoritative; routing may only over-select.
3. Dataset size must not determine required resident RAM.
4. Ingestion accepts bounded-size batches.
5. Hierarchy construction uses bounded-memory external runs and merge.
6. Hierarchy leaves identify compact deterministic addresses rather than duplicating full records.
7. Hierarchy records are ordered by deterministic mixed-radix keys and are binary-searchable where applicable.
8. Multiple applicable hierarchies may be intersected before canonical I/O.
9. Any query not fully proven by exact indexes is exact-checked against canonical data.
10. Hierarchy selection requires no semantic model or LLM.

## Original binary hierarchy record

The early page-routing Rust target used a little-endian 12-byte record:

- `key`: uint64
- `page`: uint32

Records were globally sorted by `(key,page)` and deduplicated. This deliberately simple layout supported mmap plus binary search.

The later research added exact row-level representations (`deltapost`, `densepost`, `flatpost`, `bitslice`, and sparse postings), so this page record is no longer the complete description of LHR storage.

## Canonical segmentation

Python v0 used `.npy` canonical segments for research convenience. Rust replaced this with LHR-native mmap-friendly segments while preserving stable row addressing and manifest semantics.

## Original Rust implementation priorities

- mmap hierarchy directories and canonical segments
- zero-copy/binary-searchable routing
- intersection of sorted candidate sets
- direct page-to-segment addressing
- bounded-memory external run generation and k-way merge
- compact dictionary/token encoding
- benchmark resident set size, page faults, bytes read, p50/p95 latency, build throughput, and index amplification

Most of the core retrieval/build priorities above are now implemented. Production hardening still remains for dictionaries, checksums/crash recovery, updates/compaction, concurrency, and large-scale real-machine validation.

## Why this file remains in the repository

This handoff is useful research history: it shows which invariants were considered fundamental before the Rust implementation existed. The implementation changed substantially, but the exactness, bounded-memory, deterministic-routing, and mmap goals survived.

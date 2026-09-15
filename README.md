# Localized Hierarchical Retrieval (LHR)

Experimental deterministic retrieval architecture for very large structured datasets on memory-constrained hardware.

The project began from a practical constraint: storing and querying a growing lead dataset (70M+ records) without relying on expensive analytical database infrastructure.

## Goal

Test whether expensive write-time organization plus compact localized hierarchies can make exact multi-column retrieval fast on a ~1 GB RAM VPS with disk-backed storage.

## Core hypothesis

```text
query
  -> deterministic tokenization
  -> hierarchy router
  -> smallest useful localized region
  -> binary-searchable directories
  -> progressively narrower regions
  -> exact verification against canonical rows
```

No LLM, embeddings, semantic similarity, or probabilistic retrieval is required. The engine must work even when values have no inherent meaning.

## Design principles

- Exact results; routing may over-select but cannot introduce false negatives.
- Push complexity toward ingestion rather than retrieval.
- Keep canonical data and large structures disk-backed and memory-mappable.
- Keep the query working set tiny rather than loading the database into RAM.
- Permit redundant routing metadata when it buys substantial retrieval speed.
- Avoid one duplicated row-ID posting per hierarchy wherever possible.
- Prefer compact ranges, boundaries, signatures and child addresses.
- Allow binary search on hierarchy directories.
- Make the on-disk format language-independent.

## Implementation strategy

Python is the research/reference implementation because the architecture is still changing rapidly. Once the format and algorithms stabilize, performance-critical reader/writer components can be implemented in Rust while reading the same LHR files.

## Repository

- `python/lhr/` reference implementation
- `experiments/` reproducible experiments
- `benchmarks/` performance comparisons
- `tests/` exactness tests
- `docs/ARCHITECTURE.md` design and reasoning
- `docs/FORMAT.md` proposed on-disk format

## Benchmark rule

Every performance benchmark must be checked against an exact baseline. Fast incorrect retrieval is not a successful result.

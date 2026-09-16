# Localized Hierarchical Retrieval (LHR)

LHR is an experimental deterministic retrieval architecture for very large structured datasets on memory-constrained hardware.

The project began from a practical constraint: storing and querying a growing lead dataset expected to reach 70M+ rows without relying on expensive analytical database infrastructure, while targeting a small disk-backed Linux VPS with roughly 1 GB of RAM.

The core research question is:

> Can expensive deterministic preprocessing plus compact overlapping exact/routing structures make exact multi-column retrieval fast while keeping query-time RAM and data touched extremely small?

## Current status

The production-oriented implementation is now in **Rust**. Python remains useful as historical/reference research, but the current engine, builders, exact indexes, mmap readers, external sorting, adaptive representation selection, and scale benchmarks live under `rust/`.

The current design uses:

- canonical tokenized data stored once;
- an exact singleton backbone for deterministic completeness;
- selective pair/multi-column accelerators for speed;
- adaptive exact index representations (`bitslice`, `deltapost`, `densepost`, `flatpost`, and sparse postings);
- direct compressed intersection rather than full posting materialization;
- conservative page-routing fallback when exact row indexes do not fully cover a query;
- bounded-memory external sorting/building;
- mmap-friendly on-disk structures.

No LLM, embeddings, semantic similarity, or probabilistic retrieval is required. Values and columns have no inherent meaning to the engine.

## Correctness rule

**Exactness is non-negotiable.**

Routing may over-select, but it may never exclude a true match. Exact row indexes can prove a result without canonical verification; otherwise surviving candidates are checked against canonical data.

Every performance benchmark is checked against an exact baseline. Fast incorrect retrieval is not a successful result.

## Current benchmark snapshot

These are CI/synthetic architecture-validation results, not final production guarantees.

| workload | rows | index amplification | median query | p95 | peak memory |
|---|---:|---:|---:|---:|---:|
| Hybrid-7 low-cardinality synthetic | 1M | ~1.447x | ~0.16-0.18 ms | ~0.32-0.34 ms | ~22 MB |
| Hybrid-7 low-cardinality synthetic | 10M | ~1.445x | ~1.65-1.68 ms | ~3.14-3.17 ms | ~130 MB |
| Mixed-cardinality adaptive | 1M | ~0.702x | ~0.0058 ms | ~0.203 ms | ~45 MB |
| Mixed-cardinality adaptive | 10M | ~0.500x | ~0.0266 ms | ~2.32 ms | ~279 MB |
| Lead-like 12-column workload | 5M | ~1.653x | ~0.042 ms | ~0.86 ms | ~303 MB |
| Lead-like 12-column workload | 10M | ~1.541x | ~0.062 ms | ~2.05 ms | ~522 MB |

A particularly difficult mixed-cardinality query returning roughly **2.5 million rows** dropped from about **7.25 ms** to **2.22 ms** after allowing bit-slices for the cardinality-64 field, for only a small storage increase. That experiment is one example of the project's main rule: optimize storage and query work together rather than minimizing bytes in isolation.

See [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) for the full benchmark ledger and caveats.

## Why the architecture changed over time

The current design is the result of multiple measured failures and corrections:

- materializing every hierarchy was fast but caused row-reference storage explosion;
- sparse pair graphs saved space but were incomplete as an acceleration strategy without an exact backbone;
- an exact singleton backbone restored deterministic completeness;
- base-relative block bit-packing made both storage and latency worse and was rejected;
- consecutive-gap packing fixed storage but full decompression remained expensive;
- direct compressed intersection reduced that decode cost substantially;
- explicit block fences produced too little speedup for their metadata cost;
- topology-aware profiling showed the real bottleneck was giant low-cardinality singleton composition;
- bit-sliced exact indexes removed that bottleneck;
- flat sorted postings were added for the opposite regime: sparse very-high-cardinality keyspaces.

The complete chronological reasoning, rejected designs, benchmark stages, and decisions are recorded in [`docs/RESEARCH.md`](docs/RESEARCH.md).

## Documentation

- [`docs/RESEARCH.md`](docs/RESEARCH.md) — chronological research record: what we tried, why, measured results, failures, and decisions.
- [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) — benchmark ledger, methodology, historical A/B comparisons, and current numbers.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — current architecture and query/build path.
- [`docs/FORMAT.md`](docs/FORMAT.md) — on-disk format concepts; still not fully frozen.
- [`docs/STREAMING.md`](docs/STREAMING.md) — historical page-routing/streaming prototype.
- [`docs/RUST_HANDOFF.md`](docs/RUST_HANDOFF.md) — historical contract from the Python-to-Rust transition.

## Repository layout

- `rust/` — current production-oriented implementation and benchmarks
- `python/lhr/` — reference/research implementation
- `experiments/` — reproducible research experiments
- `benchmarks/` — benchmark material
- `tests/` — correctness tests
- `docs/` — architecture, research record, formats, and benchmark history

## Design principles

- Exact deterministic results; no false negatives.
- Push complexity toward ingestion rather than retrieval.
- Keep canonical data and large structures disk-backed and mmap-friendly.
- Optimize the amount of data/work touched per query, not only theoretical operation count.
- Use overlapping access paths when their storage cost is justified.
- Treat pair/topology indexes as accelerators, never correctness dependencies.
- Choose representation by cardinality/density and measured cost, not column semantics.
- Keep the on-disk format language-independent where practical.

## Remaining production validation

The architecture is validated at 10M synthetic/lead-like scale, but the next major work is real deployment validation:

- run on the actual 1 GB Oracle VPS;
- measure warm/cold filesystem cache and block-device I/O;
- test 25M, 50M, and 70M+ rows;
- use realistic lead dictionaries/token distributions;
- measure sustained RSS, page faults, p50/p95/p99, and build throughput;
- harden update/compaction, checksums/crash recovery, concurrency, and format compatibility.

The repository should therefore be read as an active database/retrieval research project with a working Rust implementation, not as a frozen production database format.

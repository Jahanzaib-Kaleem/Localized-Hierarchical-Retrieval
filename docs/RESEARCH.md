# LHR Research Record

This document explains how Localized Hierarchical Retrieval (LHR) evolved, what was tried, what failed, which measurements changed direction, and why the current architecture looks the way it does.

It is intentionally chronological. `README.md` is the project entry point, `ARCHITECTURE.md` describes the current design, and `BENCHMARKS.md` is the compact benchmark ledger.

## 1. Original problem

The practical constraint was simple: store and query a growing structured lead dataset expected to reach roughly 70M+ rows without paying for a large analytical database service, while targeting a very small Linux VPS with about 1 GB of RAM and disk-backed storage.

The goal was never to beat every general-purpose database. The research question was narrower:

> Can expensive deterministic preprocessing plus compact, overlapping routing/index structures make exact multi-column retrieval fast while keeping query-time RAM and data touched extremely small?

The intended Oracle Always Free target used during planning is 1 OCPU / 1 GB RAM with a separate ~145 GB block volume. Build-time work may be expensive. Query-time work must remain bounded and deterministic.

## 2. Non-negotiable invariants

These rules survived every experiment:

1. Results must be deterministic and exact.
2. False negatives are unacceptable.
3. Canonical data remains authoritative and is stored once.
4. Routing/index structures may overlap, but should reference rows/regions rather than duplicate full records.
5. Dataset size should not imply that the full dataset must reside in RAM.
6. Heavy write-time preprocessing is acceptable if it reduces query work.
7. The engine must not require semantic knowledge of a column or value.
8. No embeddings, LLM semantics, or probabilistic retrieval are required.
9. Index topology is an optimization. It must never become a correctness dependency.
10. Storage amplification is measured separately from total storage: `index amplification = index bytes / canonical bytes`; total storage ratio is `1 + index amplification`.

## 3. Early Python research

The first experiments used meaningless integer columns so that success could not depend on business semantics.

### Materializing many combinations

Small experiments materialized recurring 1-, 2-, and 3-column hierarchies. Retrieval was easy, but storage exploded combinatorially. The important lesson was that storing one row reference per row per hierarchy scales roughly with `4 * rows * hierarchies` for `uint32` row IDs.

**Decision:** overlapping indexes are useful, but blindly materializing every combination is not.

### Fixed nested trees

A strict nested tree reduced duplication but made the physical organization too rigid. Different query combinations wanted incompatible orderings.

**Decision:** LHR cannot depend on one universal hierarchy or one physical row order. Multiple independent localized structures must be allowed.

### Full pair/triple portfolios

At larger synthetic scales, all-pair/all-triple portfolios proved that exact local addressing works, but repeated row references became the dominant storage cost.

**Decision:** separate routing metadata from canonical storage and make representation choice a first-class optimization problem.

### Directory and page-routing experiments

`(key,start,length)` directories, chunk-local maps, recursive localization, mmap-backed canonical data, and page signatures were explored. These established several useful facts:

- binary-searchable metadata is cheap;
- mmap can keep resident memory low;
- page-level conservative routing preserves exactness;
- bitset/page signatures can become weak when pages contain most values;
- no single physical ordering can make every overlapping hierarchy contiguous.

The Python phase established the architecture's invariants, but the production-format questions increasingly depended on byte layout and CPU cost. That triggered the move to Rust.

## 4. Rust exact-posting baseline

The first strong Rust baseline used exact row postings for every pair of 8 synthetic columns (28 pair hierarchies).

At 10M rows, the full exact-pair layout was approximately:

| metric | result |
|---|---:|
| canonical size | ~80 MB |
| total size | ~419.7 MB |
| index amplification | ~4.25x |
| median query | ~1.0 ms |
| p95 | ~3.1 ms |
| peak RSS/HWM | ~95-97 MB |
| exactness | exact |

This proved that deterministic exact lookup could be very fast and memory-light, but storage was too expensive.

**Decision:** preserve exact row-level acceleration, but attack representation and topology rather than correctness.

## 5. Sparse graph experiment: the "six degrees" idea

Instead of indexing all 28 pairs, we tested sparse graphs of pair relationships:

- graph7: 7 pair edges
- graph12: 12 pair edges
- graph16: 16 pair edges
- exactpairs: all 28

At 1M rows, graph7 reduced index amplification to about 1.07x and graph12 to about 1.84x, but uncovered 2/3-column queries often fell back to scanning essentially 100% of canonical rows. Graph16 improved median work but still had a bad uncovered tail.

**Lesson:** graph connectivity alone is not a correctness or completeness mechanism. A sparse graph can accelerate covered patterns, but some exact backbone must guarantee arbitrary predicate combinations.

## 6. Hybrid exact backbone

The next design added an exact singleton index for each column and kept only selected pair accelerators.

For 8 columns:

- Hybrid7 = 8 singleton exact indexes + 7 pair accelerators
- Hybrid12 = 8 + 12
- Hybrid16 = 8 + 16

Now every predicate could be answered exactly by singleton composition, while pair indexes were optional accelerators.

### Hybrid7 pre-compression baseline, 1M

| metric | result |
|---|---:|
| canonical | 8.000 MB |
| total | 24.662 MB |
| index amplification | 2.083x |
| median | 1.095 ms |
| p95 | 2.287 ms |
| peak memory | ~24 MB |
| canonical rows touched | 0 |
| exactness | exact |

Full 28-pair indexing was much faster per query, but Hybrid7 cut index storage by roughly half while keeping exactness.

**Decision:** Hybrid7 became the topology baseline. Exact singleton coverage is the correctness backbone; selected pairs are accelerators.

## 7. Compression experiments

Storage was still the dominant optimization target, so exact postings were compressed.

### v2: base-relative bit-packed blocks — rejected

Blocks stored one absolute base and bit-packed every row as `row - base` using a single width.

This looked compact in theory but performed badly because one large offset within a block forces a wide bit width for every value.

#### 1M Hybrid7

- index amplification: 2.883x
- median: 2.184 ms
- p95: 4.026 ms

#### 10M Hybrid7

- index amplification: 2.881x
- median: 21.56 ms
- p95: 39.60 ms
- peak memory: ~190 MB

It was worse than the uncompressed baseline in both storage and latency.

**Decision:** reject base-relative block offsets.

### v3: consecutive-gap bit packing

The format changed to store the first row in a block plus consecutive row gaps. Gaps are much smaller than offsets from the block base, so the required bit width dropped substantially.

#### 1M Hybrid7

- total: 22.077 MB
- index amplification: 1.760x
- median: ~1.42 ms
- p95: ~2.56 ms

#### 10M Hybrid7

- total: 220.57 MB
- index amplification: 1.757x
- median: 13.94 ms
- p95: 25.44 ms
- peak memory: ~155 MB

Storage improved strongly, but decoding whole postings created a serious latency cost.

**Decision:** keep consecutive-gap encoding, optimize the read path.

### Direct compressed intersection

Instead of decoding an entire posting into a temporary `Vec`, compressed blocks are intersected directly against the current sorted seed. Gaps are decoded only as the seed advances.

#### 10M Hybrid7

- storage unchanged: 1.757x index amplification
- median: 8.14 ms
- p95: 19.55 ms
- peak memory: ~142 MB

This reduced median latency by about 42% versus full v3 materialization.

**Decision:** direct streaming intersection is structurally better than full posting materialization.

## 8. Block-fence A/B test — not worth the bytes

A v4 experiment added the last row of every compressed block so whole blocks could be skipped by range.

At 10M Hybrid7:

| metric | v3 direct intersection | v4 fences |
|---|---:|---:|
| total storage | 220.57 MB | 225.26 MB |
| index amplification | ~1.757x | ~1.816x |
| median | 8.14 ms | ~7.85 ms |
| p95 | 19.55 ms | ~22.60 ms |

The median improvement was only a few percent, storage increased, and p95 was worse in that run.

**Decision:** explicit block fences were not earning their storage cost. The project returned to the v3 consecutive-gap format (`LHRDPB3`) with direct intersection.

More importantly, this experiment showed that irrelevant-block decoding was not the main remaining bottleneck.

## 9. Benchmark bias discovery

The synthetic `scale` benchmark originally generated widths and columns using coupled formulas. For Hybrid7 this accidentally meant the 2-predicate and 3-predicate cases often missed all seven pair accelerators.

That explained the seemingly alarming shape:

- 2 predicates: ~13-14 ms
- 3 predicates: ~16-17 ms
- 4 predicates: ~5 ms
- 5 predicates: ~3 ms

The benchmark was disproportionately testing the singleton-composition fallback for low widths.

This was a critical correction: the ~8 ms aggregate was not evidence that compression itself had made every query 8x slower. It was largely the cost of intersecting huge low-cardinality singleton postings when no pair accelerator covered the query.

**Decision:** add topology-aware benchmarking and measure direct-pair, pair-composed, and singleton-only paths separately.

## 10. The real bottleneck: low-cardinality singleton composition

With 10M rows and synthetic cardinalities as low as 6-20, one singleton value can represent hundreds of thousands or more than a million rows. Even compressed postings still require substantial decode/intersection work.

The representation was wrong for dense low-cardinality equality predicates.

## 11. Bit-sliced exact singletons

Low-cardinality singleton indexes were replaced adaptively with bit-sliced equality indexes.

Instead of enumerating every row ID for a value, each key bit has a bit-plane over all rows. Equality is computed word-at-a-time. When two selected indexes are both bit-sliced, the engine intersects equality masks directly before producing row IDs.

This preserved exactness while removing the giant-posting bottleneck.

### Hybrid7 after bit-slicing

At 10M rows:

- index amplification: ~1.445x
- median: ~1.65-1.68 ms
- p95: ~3.14-3.17 ms
- peak memory: ~130 MB
- canonical rows touched: 0
- exactness: exact

At 1M rows:

- index amplification: ~1.447x
- median: ~0.16-0.18 ms
- p95: ~0.32-0.34 ms
- peak memory: ~22 MB

This was the major turning point: both storage and latency improved relative to the earlier Hybrid7 baseline.

## 12. Mixed-cardinality benchmark

A more realistic adaptive test used columns with cardinalities:

`[8, 64, 1,024, 10,000, 100,000, 1,000,000]`

The engine chooses among bit-slices and posting representations based on the data/estimated work.

### Before allowing bit-slices for cardinality ~64, 10M

- index amplification: 0.487x
- median: 0.172 ms
- p95: 7.35 ms
- broad `card8 x card64` query: ~7.25 ms, returning ~2.5M rows

### After extending the bit-slice speed budget to card64, 10M

- index amplification: 0.500x
- median: 0.0266 ms
- p95: 2.32 ms
- broad `card8 x card64` query: ~2.22 ms, still returning ~2.5M rows
- peak memory: ~279 MB

The extra index storage was only a few MB at 10M rows, while the hardest broad query improved by more than 3x.

**Decision:** representation selection should consider query work, not only the smallest byte count. Slightly larger bit-slices can be the correct choice when they eliminate enormous posting intersections.

## 13. Sparse/high-cardinality exact postings

Very high-cardinality pair keyspaces create the opposite problem: dense offset tables can be wasteful even when each key has very few rows.

A flat sorted `(key,row)` representation was added for sparse/high-cardinality exact indexes. This is especially useful for pair keyspaces whose theoretical cardinality is enormous but whose observed entries are sparse.

**Decision:** LHR should not have one universal exact-posting encoding. It should select the representation that fits density/cardinality.

## 14. Lead-like stress benchmark

A 12-column synthetic workload was added with cardinalities:

`[4, 8, 32, 64, 256, 4,096, 50,000, 500,000, 2,000,000, 10,000,000, 20,000,000, 100,000,000]`

It includes exact singletons and selected low/high-cardinality pair accelerators. This is still synthetic, but is much closer to a lead database than the original 8 tiny-cardinality columns.

### 10M rows

- canonical: ~480 MB
- total: ~1.220 GB
- index amplification: ~1.541x
- median query: ~0.062 ms
- overall p95 in the latest CI run: ~2.05 ms
- peak memory: ~522 MB
- exactness checks: exact

Representative medians:

- high + high cardinality: ~0.042 ms
- low + medium: ~0.022 ms
- selected low-cardinality pair: ~0.122 ms
- sparse high-cardinality pairs: ~0.04-0.05 ms
- 3 predicates: ~0.067 ms
- 4 predicates: ~0.039 ms
- 5 predicates: ~0.063 ms
- broad low + low query returning ~312k rows: ~1.89 ms

The broad low-cardinality query is intentionally difficult because returning hundreds of thousands of exact matches has an unavoidable output/materialization component.

## 15. Current representation portfolio

The Rust engine currently supports multiple exact row-index representations:

- `bitslice` — dense low/moderate-cardinality equality filtering
- `deltapost` — consecutive-gap bit-packed postings
- `densepost` — dense keyspace offset representation
- `flatpost` — sparse/high-cardinality sorted `(key,row)` representation
- `postings` — general sparse posting fallback

It also retains page-level hierarchy types for conservative fallback routing.

The exact-index builder evaluates representation candidates rather than hard-coding one format for every column.

## 16. Current query strategy

For a query:

1. Validate/deduplicate predicates and reject impossible tokens immediately.
2. Find exact row hierarchies fully contained in the query.
3. Estimate each candidate by row count and coverage gain.
4. Select a compact covering set and sort it by estimated candidate count.
5. If the first two selected structures are bit-sliced, intersect their equality masks directly.
6. Intersect remaining exact structures against the shrinking row seed.
7. If every predicate is exactly covered, return the exact hit count without canonical verification.
8. If exact row indexes do not fully cover the query, fall back to conservative page hierarchies and exact canonical verification.

This preserves the original LHR rule: fast indexes may accelerate, but correctness never depends on an approximate route.

## 17. What the project learned

The largest lessons so far are:

- The fundamental optimization target is **bytes/work touched per query**, not theoretical operation count.
- Exactness and sparse topology are compatible if there is a complete exact backbone.
- Compression that minimizes bytes can still be the wrong representation if it creates too much decode work.
- Low-cardinality equality predicates want word-parallel representations; high-cardinality sparse predicates want compact postings.
- Query workload/topology matters. Aggregate medians can hide structurally different paths.
- Benchmarks must be checked for accidental workload bias before architectural conclusions are drawn.
- Small representation-specific storage increases are worthwhile when they remove a dominant CPU path.
- The engine should adapt representation by cardinality/density rather than assigning semantic meaning to fields.

## 18. Important caveats

The strongest numbers are still CI/synthetic benchmarks. They are evidence that the architecture works, not proof of final production behavior.

Remaining validation:

- actual 1 GB Oracle VPS behavior;
- cold-cache and block-device I/O;
- sustained RSS/page-fault behavior under repeated queries;
- realistic lead dictionaries and token widths;
- 25M, 50M, and 70M+ row builds;
- update/compaction strategy;
- crash recovery/checksums/format freeze;
- concurrent readers/writers;
- real query-frequency-driven accelerator selection.

The 10M lead-like canonical row is 12 `u32` tokens (~48 bytes/row), which is far more realistic than the original 8-byte synthetic row, but it is still not a full real lead record with dictionaries and external fields.

## 19. Development milestones

Important experiment/implementation milestones include:

- `91c8e58...` — consecutive-gap packed postings (v3)
- `d044b6d...` — direct compressed intersection without full posting materialization
- `1d03cc7...` — block-fence experiment used for the A/B test; later not retained as the primary format
- PR #3 — compressed/adaptive exact-posting work merged into `main`
- PR #4 — scale hardening, sparse flat postings, lead-like benchmarks, and medium-cardinality bit-slice tuning merged into `main`

The current merged code should be treated as the implementation reference; this document records the reasoning that produced it.

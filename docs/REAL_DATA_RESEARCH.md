# LHR Real-Data Research Record

This document begins the real-data phase of LHR research. It intentionally follows the same discipline as `RESEARCH.md`: preserve the baseline, describe failures as observed, inspect the mechanism, test one hypothesis at a time, and record decisions even when an experiment is rejected.

The compact raw baseline is preserved in [`../benchmarks/REAL_DATA_SHOPIFY_1_9M_BASELINE.md`](../benchmarks/REAL_DATA_SHOPIFY_1_9M_BASELINE.md).

## 1. Why this phase exists

Until 2026-09-17, LHR's strongest performance evidence came from synthetic workloads. Those runs were useful because they isolated cardinality, topology and representation behavior, but they could not answer several important questions:

- whether real text dictionaries and skewed business data would preserve the synthetic results;
- whether first-hit / warm-hit behavior would diverge materially;
- whether cursor and materialization paths behaved sensibly on a naturally broad result set;
- whether process residency and block-device faults would expose costs hidden by CI benchmarks;
- whether an apparently complete product query surface still contained unaccelerated paths.

The first real Shopify dataset is therefore treated as a validation instrument, not a marketing benchmark.

## 2. North-star constraint

The working target is now at least **1 TB of structured data on a VPS with roughly 1 GB RAM**.

This strengthens rather than changes the original invariants:

1. exact deterministic results;
2. no false negatives;
3. canonical data remains authoritative;
4. RAM should scale with query working set, not total database size;
5. bytes/data touched per query are first-class optimization targets;
6. storage amplification must be measured alongside latency;
7. expensive bounded preprocessing is acceptable when it reduces repeated query work;
8. accelerators are optional performance structures, never correctness dependencies;
9. representation choice follows distributions/work, not column semantics;
10. claims must lag evidence.

A 1 TB target also makes any algorithm that materializes work proportional to a huge result prefix suspect even when it looks harmless at 1.9M rows.

## 3. First real dataset

Shopify store data from `growthenginenowoslawski/shopify-master-list` was imported into one sealed immutable generation.

Observed generation:

- 1,902,012 rows
- 16 columns
- 1,858 pages
- 118 segments
- 121,731,600 canonical bytes
- 51,381,493 routing/index bytes
- 375,619,739 total bytes

Important limitation: the whole generation is smaller than the VPS's physical RAM. This is a real-distribution test, not proof of the final disk-resident scale thesis.

## 4. What the real run validated

Several independent exact paths survived contact with real data:

- unique high-cardinality `densepost`: 7 µs warm median;
- non-unique `densepost`: 32 µs warm median for 13 matches;
- `deltapost` numeric equality: 296 µs median for 1,865 matches;
- mixed `deltapost` + `bitslice` intersection: 360 µs median;
- broad four-way exact intersection: 8.53 ms median with ~791k final candidates;
- first 1,000 rows of a ~1.9M-hit equality result: 4.69 ms median.

These results are not universal latency claims. They establish that the adaptive exact-index architecture remains effective on at least one non-synthetic distribution.

**Decision:** preserve the representation portfolio and planner assumptions. The real-data failures should be fixed locally rather than used as evidence to replace the exact-index core.

## 5. Failure discovered: numeric range fallback

A narrow range on `estimated_monthly_visits` (`10000..10100`) examined all 1,902,012 visible rows and took about 4.7 seconds.

Initial suspicion included the possibility that a low-storage import configuration had omitted a useful range structure.

Source inspection resolved the ambiguity: the typed API explicitly sends any filter set that is not pure equality through a deterministic `for_each_visible_row` fallback. The existing `deltapost` singleton is an equality posting representation; the query API does not currently use it as an ordered range structure.

This is therefore a **known completeness-first implementation gap**, not a corrupt index and not a planner accident.

**Decision:** do not add a range structure blindly. Treat exact numeric range acceleration as a new A/B research problem. Any proposal must record added bytes, build cost, range latency, equality regression, RAM, faults/reads and correctness.

## 6. Failure discovered: deep equality pagination

The broad equality query returned its first 1,000 rows in ~4.7 ms. The same query around cursor 100k took ~3.28 seconds and increased process RSS by ~148 MiB during the measured call. A cursor around 1.5M exceeded the 30-second query timeout.

Source inspection found a direct mechanism:

1. the typed query API requested an initial prefix;
2. if too few returned rows were beyond `after_row_id`, it doubled the requested prefix;
3. it reran the equality query from the beginning;
4. it repeated until the requested cursor was inside the materialized prefix;
5. it discarded all rows at or before the cursor.

The behavior was correct but made work grow with cursor depth.

The stable row-ID layer already guarantees that logical row IDs are strictly increasing with physical position inside each physical layer. That invariant allows a cursor to be converted to a physical lower bound by binary search.

### Hypothesis A — lower-bound cursor seek with no new storage

Instead of replaying prefixes:

- binary-search the row-ID map for the first physical row whose logical ID is greater than the cursor;
- pass that lower bound into the exact row result collector;
- binary-search the already sorted exact candidate vector before taking the requested page;
- apply the same lower bound independently in base and delta layers;
- preserve total hit count and visibility semantics.

Expected properties:

- zero index/storage amplification;
- deep page latency no longer grows because of geometric prefix replay;
- stable logical IDs preserved across explicit row-ID maps and compaction;
- delta visibility/tombstones preserved;
- current broad exact path still materializes the complete candidate vector, so this is **not yet the final TB-scale streaming solution**.

Implementation is on the `real-data-research` branch with targeted base/delta cursor tests. It is not accepted until CI passes and the real Shopify pagination matrix is rerun.

**Status:** implementation pending validation.

## 7. Remaining pagination concern after Hypothesis A

Even after removing prefix replay, `candidate_rows_with_lookups` currently produces a complete `Vec<u32>` for the final exact candidate set before the caller takes a limited page.

At 1.9M rows this can be fast and manageable. At the 1 TB target, a query with hundreds of millions or billions of matches cannot be allowed to require a correspondingly large transient candidate vector merely to return a 1,000-row page.

**Next research question:** can exact posting/bit-slice composition expose a sorted seekable iterator (or bounded chunk stream) that supports `lower_bound(row_id)` and limited output without materializing the full final set?

This should be tested only after Hypothesis A establishes the simpler lower-bound baseline; otherwise two independent changes would be confounded.

## 8. Memory-residency observation

After deep-pagination stress, process RSS remained around 546 MiB versus a much smaller early settled process. A later diagnostic check showed ~318 MiB and no active generation leases.

This means the current evidence is insufficient to call the behavior a leak. Candidate explanations include allocator retention, mmap residency and filesystem page residency.

**Decision:** add peak/system-level measurement before changing memory-management code. Future dedicated runs should capture `/proc/<pid>/smaps_rollup`, peak RSS/HWM, `vmstat`, `iostat` and whole-system memory alongside MCP counters when practical.

## 9. Telemetry semantic issue

The typed set/range fallback reports `pages_touched = 0` even though it iterates every visible row. Therefore the field is not currently a physical-I/O counter across all query routes.

**Decision:** before publishing a real-data benchmark table as a general product claim, either make the metric route-consistent or document/rename it so zero cannot be interpreted as zero physical memory/storage pages accessed.

## 10. Acceptance discipline for this phase

For each optimization:

1. preserve the pre-change baseline;
2. state the mechanism being changed;
3. make one architectural change at a time where practical;
4. prove exactness with tests/reference checks;
5. record storage before/after;
6. record first-hit and warm p50/p95/p99;
7. record RSS/peak RSS, faults and process reads where available;
8. test adversarial breadth/depth, not only selective lookups;
9. reject improvements whose byte/RAM cost does not earn its latency benefit;
10. keep rejected experiments in the research record.

The next major scale milestone should be a real dataset materially larger than RAM (initially 5–10 GB on the same 1 GB VPS), followed by progressively larger real runs toward the 1 TB target.

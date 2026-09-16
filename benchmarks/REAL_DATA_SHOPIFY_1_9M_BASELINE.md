# Shopify 1.9M Real-Data Baseline — 2026-09-17

This is the first non-synthetic LHR benchmark record. It is preserved as a pre-optimization baseline rather than rewritten after fixes.

Source dataset: `growthenginenowoslawski/shopify-master-list`

Runtime target: production LHR Docker/MCP instance on a VPS with approximately 1 GiB RAM.

## Dataset / generation

- generation: `1`
- sealed: yes
- rows: `1,902,012`
- columns: `16`
- pages: `1,858`
- segments: `118`
- exact hierarchies: `16`
- canonical bytes: `121,731,600`
- routing/index bytes: `51,381,493`
- total generation bytes: `375,619,739`
- integrity manifest: present

The generation is only about 376 MB, so this run validates real-data behavior but does **not** validate the final disk-first / 1 TB-on-1-GB goal. The complete generation is smaller than physical RAM.

## Adaptive singleton representations observed

| column | cardinality | representation |
|---|---:|---|
| domain | 1,902,012 | densepost |
| merchant_name | 1,831,649 | densepost |
| platform | 2 | bitslice |
| status | 2 | bitslice |
| estimated_monthly_visits | 24,856 | deltapost |
| products_sold | 16,195 | deltapost |
| employee_count | 1,870 | deltapost |
| employee_count_bucket | 12 | bitslice |
| tiktok_followers | 12,400 | deltapost |
| ecom_verdict | 2 | bitslice |
| tags | 23 | bitslice |
| technologies | 643,415 | densepost |

The effectively empty fields also received exact singleton coverage but are not useful benchmark dimensions.

## Query baseline

| workload | result/candidates | first observed | median | p95 | process RSS during measured run | important notes |
|---|---:|---:|---:|---:|---:|---|
| `domain = 0000studios.com` | 1 | 43 µs | 7 µs | 17 µs | ~49 MiB | densepost, exact row proof, no canonical verification |
| `merchant_name = Home` | 13 | 40.187 ms | 32 µs | 65 µs | ~53 MiB | cold-ish first hit much slower than warm path |
| `products_sold = 158` | 1,865 | 131.4 ms | 296 µs | 383 µs | ~63 MiB | deltapost equality |
| broad four-predicate exact intersection | 791,552 | — | 8.53 ms | 57.75 ms | ~64 MiB | mostly bitslice |
| mixed deltapost + 3 bitslices | 884 | — | 360 µs | 386 µs | ~69 MiB | exact mixed representations compose efficiently |
| visits range `10000..10100` | full 1,902,012 scan | 4.788 s | 4.717 s | 4.916 s | ~323 MiB | no hierarchy lookup; deterministic canonical fallback |
| broad equality, first 1,000 rows | 1,902,012 hits | — | 4.690 ms | 4.709 ms | ~127 MiB | optimized equality route |
| same broad equality, cursor ~100k | 1,902,012 hits | 3.282 s | 3.282 s* | 3.282 s* | ~562→710 MiB | +148 MiB RSS, +717 major faults, +24.5 MiB process reads |
| same broad equality, cursor ~1.5M | — | >30 s | timeout | timeout | — | query timeout reached |

`*` One measured sample only.

## Deep-pagination raw resource counters

Cursor approximately 100,000, limit 1,000.

Before:

- major faults: `14,080`
- minor faults: `346,032`
- read bytes: `910,307,328`
- RSS: `589,647,872`
- write bytes: `32,768`

After:

- major faults: `14,797`
- minor faults: `352,236`
- read bytes: `936,034,304`
- RSS: `744,873,984`
- write bytes: `32,768`

Authoritative deltas:

- major faults: `+717`
- minor faults: `+6,204`
- read bytes: `+25,726,976` (~24.5 MiB)
- RSS: `+155,226,112` (~148 MiB)
- writes: `0`

A previous conversational summary said roughly 707 major faults. The raw subtraction above is authoritative.

## Memory caveat / later observation

Immediately after the deep-pagination stress, process RSS remained around 546 MiB. A later diagnostic check showed approximately 318 MiB RSS and no active generation leases. This weakens the hypothesis of a permanently pinned snapshot/leak, but does not explain the residency behavior. Peak RSS was not continuously sampled.

## Counter caveats

- benchmark RSS delta is before/after RSS, not peak RSS;
- process RSS is not whole-VPS memory;
- zero read-byte delta on a warm run does not imply no memory pages were accessed;
- `rows_examined` primarily represents canonical verification/scan work, not all index work;
- `pages_touched` currently has route-specific semantics and the typed range fallback reports zero even while scanning visible rows; do not publish it as physical page I/O without qualification;
- CPU percentage and kernel page cache are not exposed by the current MCP diagnostics.

## Source-level explanation discovered after the run

Two benchmark failures were reproduced in source inspection:

1. **Range/set predicates:** the typed query API deliberately routes non-pure-equality filters through exact `for_each_visible_row` fallback. This is a correctness-first implementation, not an import/configuration failure.
2. **Equality deep pagination:** the typed API repeatedly requested a geometrically growing prefix, then discarded rows before `after_row_id`. This makes cursor depth directly increase work and explains the 100k / 1.5M collapse.

The pagination behavior is being fixed first because it can be removed without adding index storage. The range path remains an explicit accelerator research problem and must be evaluated on latency, bytes, RAM, I/O and exactness together.

## Benchmark discipline for follow-up

Do not overwrite this baseline. Record post-change results separately and compare at minimum:

- cursor 0
- cursor 1k
- cursor 10k
- cursor 100k
- cursor 500k
- cursor 1M
- cursor 1.5M

For range accelerators record canonical bytes, index/routing bytes, total bytes, storage amplification, first-hit and warm latency, RSS/peak RSS where possible, page faults, process reads, rows examined and exact correctness.

# Streaming hierarchy prototype (Historical)

> **Historical prototype.** This document records the page-routing stage that preceded the current adaptive exact-row architecture. The ideas remain relevant as a conservative fallback path, but the current engine can often answer fully covered queries from exact row indexes without touching canonical pages. See [`ARCHITECTURE.md`](ARCHITECTURE.md) and [`RESEARCH.md`](RESEARCH.md).

This stage introduced the first builder whose memory requirement was bounded by input batch/page size rather than total dataset size.

## Representation

A page hierarchy stores sorted `(composite_key, page_id)` entries rather than a row ID for every matching row. Composite keys use deterministic mixed-radix integer encoding. Because entries are sorted, the reader uses binary search to locate the exact range of pages associated with a key.

For queries covered by multiple page hierarchies, page sets can be intersected before canonical data is touched. Exact predicates are then evaluated only inside surviving pages.

## Correctness

Page routing is conservative: a page is listed whenever the hierarchy key occurs anywhere in the page. Routing can therefore over-select pages but cannot remove a true match. Canonical verification remains authoritative whenever the exact row-index planner cannot fully prove the query.

## Why this differed from conventional postings

A conventional materialized hierarchy can cost roughly one row reference per row per hierarchy. This prototype moved the reference boundary upward: repeated references identified pages rather than individual rows. Page size therefore became an explicit storage/selectivity knob.

## What happened next

The page-routing design solved bounded-memory and correctness problems but could still touch too many canonical rows for common exact predicates. Later Rust research therefore added an exact singleton backbone and selective exact multi-column accelerators.

That work introduced adaptive row-index representations:

- consecutive-gap compressed postings;
- direct streaming compressed intersection;
- bit-sliced equality indexes for low/moderate cardinalities;
- dense posting directories where keyspace addressing is efficient;
- flat sorted postings for sparse high-cardinality keyspaces.

Page routing was retained as a fallback rather than discarded.

## Historical bottlenecks

At this stage, the Python writer accumulated hierarchy page records before final serialization, and page-to-segment lookup was not fully optimized. The subsequent Rust builder moved hierarchy construction to bounded-memory external sorting and the native reader gained direct manifest/segment addressing.

This file remains in the repository because it captures an important intermediate conclusion: **routing can be conservative and page-granular without sacrificing exactness, but page routing alone is not always selective enough for the desired latency target.**

# Streaming hierarchy prototype

This branch introduces the first builder whose memory requirement is bounded by input batch/page size rather than total dataset size.

## Representation

A hierarchy does **not** store a row ID for every matching row. It stores sorted `(composite_key, page_id)` entries. Composite keys use deterministic mixed-radix integer encoding. Because entries are sorted, the reader uses two binary searches to locate the exact range of pages associated with a key.

For queries covered by multiple hierarchies, page sets are intersected before canonical data is touched. Exact predicates are then evaluated only inside surviving pages.

## Correctness

Page routing is conservative: a page is listed whenever the hierarchy key occurs anywhere in the page. Therefore routing can over-select pages but cannot remove a true match. Canonical verification remains authoritative.

## Why this is different from conventional postings

A conventional materialized hierarchy can cost roughly one row reference per row per hierarchy. This prototype moves the reference boundary upward: repeated references identify pages rather than individual rows. Page size therefore becomes an explicit storage/selectivity knob.

## Known bottleneck

The current writer accumulates hierarchy page records in Python lists before final serialization. Canonical ingestion itself is bounded, but hierarchy finalization is not yet fully external-memory. The next builder should emit sorted runs incrementally and perform a k-way external merge so hierarchy construction remains bounded even at 100M+ rows.

The reader also performs a sequential page-to-segment mapping in the reference implementation. The format should gain a direct page table so candidate page IDs translate to `(segment, offset, length)` without walking unrelated pages.

These limitations are intentionally documented rather than hidden by small synthetic benchmarks.

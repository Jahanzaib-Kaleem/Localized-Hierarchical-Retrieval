# LHR Benchmark Ledger

This file is the compact benchmark record. For the chronological reasoning behind each change, see [`RESEARCH.md`](RESEARCH.md).

## Definitions

- **Canonical**: authoritative tokenized rows only, excluding routing/index files.
- **Index amplification**: `(total_bytes - canonical_bytes) / canonical_bytes`.
- **Total storage ratio**: `1 + index amplification`.
- **Exact**: benchmark results are checked against an exact scan/baseline for sampled queries.
- **Rows/pages touched = 0**: exact row indexes fully proved the result, so canonical verification was unnecessary for that query.

CI timings are useful for relative comparisons but are not dedicated-hardware latency guarantees. Small run-to-run differences are expected.

## Current merged benchmarks

### Hybrid-7 synthetic

Eight low-cardinality synthetic columns, exact singleton backbone plus seven pair accelerators.

| rows | canonical | total | index amp | median | p95 | peak memory | exact |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1M | ~8.00 MB | ~19.58 MB | 1.447x | ~0.16-0.18 ms | ~0.32-0.34 ms | ~22 MB | yes |
| 10M | ~80.00 MB | ~195.59 MB | 1.445x | ~1.65-1.68 ms | ~3.14-3.17 ms | ~130 MB | yes |

Latest 10M width medians were roughly:

| predicates | median |
|---:|---:|
| 2 | 1.64 ms |
| 3 | 1.88 ms |
| 4 | 1.51 ms |
| 5 | 1.83 ms |

The topology-aware 10M benchmark separates route type:

| route | median | p95 |
|---|---:|---:|
| 2-predicate direct pair | ~0.52 ms | ~2.22 ms |
| 2-predicate singleton-only | ~1.72 ms | ~2.11 ms |
| 3-predicate pair-composed | ~1.62 ms | ~5.45 ms |
| 3-predicate singleton-only | ~2.34 ms | ~2.84 ms |
| 4-predicate pair-composed | ~2.15 ms | ~5.91 ms |
| 4-predicate singleton-only | ~2.47 ms | ~3.41 ms |
| 5-predicate pair-composed | ~1.76 ms | ~4.44 ms |

## Mixed-cardinality adaptive workload

Cardinalities:

`[8, 64, 1,024, 10,000, 100,000, 1,000,000]`

### 1M rows

| metric | result |
|---|---:|
| canonical | ~24.00 MB |
| total | ~40.84 MB |
| index amplification | 0.702x |
| median | 0.0058 ms |
| p95 | 0.203 ms |
| peak memory | ~45 MB |
| exactness | exact |

### 10M rows

| metric | result |
|---|---:|
| canonical | ~240.00 MB |
| total | ~360.06 MB |
| index amplification | 0.500x |
| median | 0.0266 ms |
| p95 | 2.32 ms |
| peak memory | ~279 MB |
| exactness | exact |

10M pattern medians:

| pattern | typical hits | median |
|---|---:|---:|
| card8 + card64 | ~2.5M | ~2.22 ms |
| card8 + card1M | ~2 | ~0.009 ms |
| card64 + card10k | ~290 | ~0.016 ms |
| card1024 + card100k | ~1 | ~0.036 ms |
| 3 predicates | ~1 | ~0.030 ms |
| 4 predicates | ~1 | ~0.017 ms |

The broad card8/card64 case is intentionally difficult because it returns millions of exact matches.

## Lead-like workload

Cardinalities:

`[4, 8, 32, 64, 256, 4,096, 50,000, 500,000, 2,000,000, 10,000,000, 20,000,000, 100,000,000]`

This workload exercises low-cardinality filters, medium-cardinality filters, very sparse high-cardinality fields, selected pair accelerators, and mixed-width queries.

### 5M rows

| metric | result |
|---|---:|
| canonical | ~240.00 MB |
| total | ~636.74 MB |
| index amplification | 1.653x |
| median | ~0.042 ms |
| p95 | ~0.86 ms |
| peak memory | ~303 MB |
| exactness | exact |

### 10M rows

| metric | result |
|---|---:|
| canonical | ~480.00 MB |
| total | ~1,219.70 MB |
| index amplification | 1.541x |
| median | ~0.062 ms |
| overall p95 | ~2.05 ms |
| peak memory | ~522 MB |
| exactness | exact |

Representative 10M pattern medians:

| pattern | median hits | median latency |
|---|---:|---:|
| high + high cardinality | 1 | ~0.042 ms |
| low + medium | ~5 | ~0.022 ms |
| selected low pair | ~39k | ~0.122 ms |
| sparse high-cardinality pair | 1 | ~0.04-0.05 ms |
| 3 predicates | 1 | ~0.067 ms |
| 4 predicates | 1 | ~0.039 ms |
| 5 predicates | 1 | ~0.063 ms |
| broad low + low | ~312k | ~1.89 ms |

The latest 10M lead-like CI pattern buckets used only five samples each, so maximum/p95 values inside an individual pattern are especially sensitive to shared-runner jitter. Medians are more representative until dedicated-machine runs are available.

## Historical decision benchmarks

These numbers explain major architectural changes.

### Full 28 exact pairs, pre-delta, 10M

| metric | result |
|---|---:|
| canonical | ~80 MB |
| total | ~419.7 MB |
| index amplification | ~4.25x |
| median | ~1.0 ms |
| p95 | ~3.1 ms |
| peak memory | ~95-97 MB |

**Interpretation:** very fast and memory-light, but too much index storage.

### Hybrid7 before packed compression, 1M

| metric | result |
|---|---:|
| total | 24.662 MB |
| index amplification | 2.083x |
| median | 1.095 ms |
| p95 | 2.287 ms |
| peak memory | ~24 MB |

**Interpretation:** sparse pair accelerators plus exact singleton backbone preserved correctness with much lower storage.

### Packed v2 base-relative blocks — regression

| rows | index amp | median | p95 | peak memory |
|---:|---:|---:|---:|---:|
| 1M | 2.883x | 2.184 ms | 4.026 ms | ~28 MB |
| 10M | 2.881x | 21.56 ms | 39.60 ms | ~190 MB |

**Interpretation:** one wide offset forced large block bit widths. Worse storage and latency; rejected.

### Packed v3 consecutive gaps, full posting decode

| rows | index amp | median | p95 | peak memory |
|---:|---:|---:|---:|---:|
| 1M | 1.760x | ~1.42 ms | ~2.56 ms | ~23 MB |
| 10M | 1.757x | 13.94 ms | 25.44 ms | ~155 MB |

**Interpretation:** storage win, CPU regression from materializing/decompressing full postings.

### v3 direct compressed intersection

| rows | index amp | median | p95 | peak memory |
|---:|---:|---:|---:|---:|
| 1M | 1.760x | ~0.78-0.94 ms | ~1.9-2.2 ms | ~23 MB |
| 10M | 1.757x | 8.14 ms | 19.55 ms | ~142 MB |

**Interpretation:** ~42% lower 10M median with identical storage by avoiding full posting materialization.

### Explicit block fences A/B, 10M

| metric | v3 streaming | v4 fences |
|---|---:|---:|
| total | 220.57 MB | 225.26 MB |
| index amp | 1.757x | 1.816x |
| median | 8.14 ms | ~7.85 ms |
| p95 | 19.55 ms | ~22.60 ms |

**Interpretation:** small median win did not justify more bytes and worse p95. The dominant cost was elsewhere.

### Bit-slice turning point, Hybrid7 10M

| metric | before low-cardinality bit-slice solution | current adaptive result |
|---|---:|---:|
| index amp | ~1.76x | ~1.445x |
| median | 8.14 ms | ~1.68 ms |
| p95 | 19.55 ms | ~3.17 ms |
| peak memory | ~142 MB | ~130 MB |

**Interpretation:** the principal problem was giant low-cardinality singleton composition, not compression itself.

### Card64 bit-slice decision, mixed 10M

| metric | card64 postings | card64 bit-slice |
|---|---:|---:|
| index amp | 0.487x | 0.500x |
| overall median | 0.172 ms | 0.0266 ms |
| overall p95 | 7.35 ms | 2.32 ms |
| broad 2.5M-hit query | 7.25 ms | 2.22 ms |

**Interpretation:** a tiny storage increase was worth a >3x improvement on the dominant broad-query tail.

## Representation choices observed in current benchmarks

The adaptive exact builder may choose:

- `bitslice` for dense low/moderate-cardinality singletons;
- `deltapost` for compressible sorted row lists;
- `densepost` where keyspace addressing is efficient;
- `flatpost` for sparse very-high-cardinality keyspaces;
- `postings` as a general fallback.

Selection is driven by deterministic dimensions and measured/estimated bytes, with an explicit query-work allowance for bit-slices.

## Benchmark discipline

A performance result is not accepted unless correctness remains exact. Benchmark design must also avoid coupling query generation to index topology in a way that accidentally over- or under-samples specific paths.

Before production claims, repeat the important suites on:

1. the actual 1 GB Oracle VPS;
2. warm and cold filesystem cache;
3. 25M / 50M / 70M+ rows;
4. realistic dictionary/token distributions;
5. repeated runs sufficient for stable p50/p95/p99 and page-fault/RSS measurements.

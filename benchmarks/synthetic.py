"""Reproducible LHR baseline benchmark.

Usage from repository root:
    python benchmarks/synthetic.py --rows 1000000
"""
import argparse
import shutil
import tempfile
import time
from pathlib import Path
import sys

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from python.lhr.engine import Dataset, Predicate, build_dataset


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rows", type=int, default=1_000_000)
    parser.add_argument("--queries", type=int, default=200)
    parser.add_argument("--page-rows", type=int, default=256)
    args = parser.parse_args()

    rng = np.random.default_rng(42)
    cards = np.array([8, 12, 6, 16, 10, 20, 8, 14], dtype=np.uint8)
    rows = np.empty((args.rows, len(cards)), dtype=np.uint8)
    for c, card in enumerate(cards):
        rows[:, c] = rng.integers(0, int(card), size=args.rows, dtype=np.uint8)

    root = Path(tempfile.mkdtemp(prefix="lhr-benchmark-"))
    try:
        t0 = time.perf_counter()
        build_dataset(rows, root, page_rows=args.page_rows)
        build_seconds = time.perf_counter() - t0
        ds = Dataset(root)

        latencies = []
        correct = 0
        for _ in range(args.queries):
            source = int(rng.integers(0, args.rows))
            cols = rng.choice(len(cards), size=3, replace=False)
            query = tuple(Predicate(int(c), int(rows[source, c])) for c in cols)

            t0 = time.perf_counter()
            actual = ds.query(query)
            latencies.append((time.perf_counter() - t0) * 1000)

            mask = np.ones(args.rows, dtype=bool)
            for p in query:
                mask &= rows[:, p.column] == p.value
            expected = np.flatnonzero(mask).astype(np.uint32)
            correct += int(np.array_equal(actual, expected))

        print(f"rows={args.rows:,}")
        print(f"build_seconds={build_seconds:.3f}")
        print(f"median_query_ms={np.median(latencies):.3f}")
        print(f"p95_query_ms={np.quantile(latencies, .95):.3f}")
        print(f"correct={correct}/{args.queries}")
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()

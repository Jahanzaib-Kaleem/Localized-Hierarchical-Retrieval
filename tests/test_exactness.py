import numpy as np

from python.lhr.engine import Dataset, Predicate, build_dataset


def test_query_matches_full_scan(tmp_path):
    rng = np.random.default_rng(7)
    rows = rng.integers(0, 12, size=(10_000, 6), dtype=np.uint8)
    build_dataset(rows, tmp_path, page_rows=128)
    ds = Dataset(tmp_path)

    queries = [
        (Predicate(0, 3),),
        (Predicate(1, 7), Predicate(4, 2)),
        (Predicate(0, 1), Predicate(2, 4), Predicate(5, 8)),
    ]

    for query in queries:
        actual = ds.query(query)
        mask = np.ones(len(rows), dtype=bool)
        for p in query:
            mask &= rows[:, p.column] == p.value
        expected = np.flatnonzero(mask).astype(np.uint32)
        assert np.array_equal(actual, expected)


def test_missing_value_returns_empty(tmp_path):
    rows = np.zeros((1_000, 3), dtype=np.uint8)
    build_dataset(rows, tmp_path)
    ds = Dataset(tmp_path)
    assert len(ds.query([Predicate(1, 5)])) == 0

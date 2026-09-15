use lhr::{build_u8_batches, BuildConfig, Engine, HierarchySpec, Predicate};

fn exact_count(rows: &[u8], cols: usize, q: &[Predicate]) -> u64 {
    rows.chunks_exact(cols).filter(|row| q.iter().all(|p| row[p.column] as u64 == p.value)).count() as u64
}

fn exact_ids(rows: &[u8], cols: usize, q: &[Predicate]) -> Vec<u64> {
    rows.chunks_exact(cols).enumerate().filter(|(_, row)| q.iter().all(|p| row[p.column] as u64 == p.value)).map(|(i,_)| i as u64).collect()
}

#[test]
fn native_builder_and_reader_are_exact_across_irregular_batches() {
    let d = tempfile::tempdir().unwrap();
    let cards = vec![8u64, 12, 6, 16, 10, 20];
    let cols = cards.len();
    let n = 25_003usize;
    let mut rows = Vec::with_capacity(n * cols);
    for r in 0..n {
        for (c, &k) in cards.iter().enumerate() {
            rows.push(((r * (c + 3) + r / 17 + c * 7) % k as usize) as u8);
        }
    }
    let split1 = 3_001 * cols;
    let split2 = 17_777 * cols;
    let batches = vec![rows[..split1].to_vec(), rows[split1..split2].to_vec(), rows[split2..].to_vec()];
    let cfg = BuildConfig {
        columns: cols,
        page_rows: 127,
        cardinalities: cards,
        hierarchies: vec![
            HierarchySpec { columns: vec![0, 1] },
            HierarchySpec { columns: vec![2, 3] },
            HierarchySpec { columns: vec![0, 2, 4] },
            HierarchySpec { columns: vec![1, 3, 5] },
        ],
        max_sort_records: 41,
    };
    let manifest = build_u8_batches(batches, d.path(), &cfg).unwrap();
    assert_eq!(manifest.rows, n as u64);
    assert!(manifest.hierarchies.iter().all(|h| h.kind == "bitmap" || h.kind == "sparse"));
    let engine = Engine::open(d.path()).unwrap();

    for seed in 0..200usize {
        let r = (seed * 7919 + 13) % n;
        let width = 2 + seed % 4;
        let mut q = Vec::new();
        for j in 0..width {
            let c = (seed + j * 2) % cols;
            let v = rows[r * cols + c] as u64;
            if !q.iter().any(|p: &Predicate| p.column == c) { q.push(Predicate { column: c, value: v }); }
        }
        let expected = exact_count(&rows, cols, &q);
        let stats = engine.query(&q);
        assert_eq!(stats.hits, expected, "query {seed:?}");
        assert!(stats.rows_checked <= n as u64);
    }

    let q = [Predicate { column: 0, value: 3 }, Predicate { column: 2, value: 1 }, Predicate { column: 4, value: 7 }];
    let expected = exact_ids(&rows, cols, &q);
    let (ids, stats) = engine.query_row_ids(&q, 17);
    assert_eq!(stats.hits, expected.len() as u64);
    assert_eq!(ids, expected.iter().copied().take(17).collect::<Vec<_>>());
    for id in ids {
        let row = engine.row(id).unwrap();
        assert!(q.iter().all(|p| row[p.column] == p.value));
    }
}

#[test]
fn impossible_token_short_circuits_without_canonical_io() {
    let d = tempfile::tempdir().unwrap();
    let cfg = BuildConfig {
        columns: 2,
        page_rows: 64,
        cardinalities: vec![4, 4],
        hierarchies: vec![HierarchySpec { columns: vec![0, 1] }],
        max_sort_records: 8,
    };
    build_u8_batches(vec![vec![0u8; 1000 * 2]], d.path(), &cfg).unwrap();
    let engine = Engine::open(d.path()).unwrap();
    let stats = engine.query(&[Predicate { column: 0, value: 99 }]);
    assert_eq!(stats.hits, 0);
    assert_eq!(stats.rows_checked, 0);
    assert_eq!(stats.pages_touched, 0);
}

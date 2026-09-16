use lhr::{add_exact_hierarchies, build_u32_batches, BuildConfig, Engine, HierarchySpec, Predicate};

#[test]
fn low_cardinality_singletons_use_bitslices_and_high_cardinality_stay_postings() {
    let d = tempfile::tempdir().unwrap();
    let rows = 40_003usize;
    let cards = vec![8u64, 100_000u64];
    let mut data = Vec::with_capacity(rows * 2);
    for r in 0..rows {
        data.push((r % 8) as u32);
        data.push(((r * 997 + 31) % 100_000) as u32);
    }

    let cfg = BuildConfig {
        columns: 2,
        page_rows: 257,
        cardinalities: cards,
        hierarchies: vec![],
        max_sort_records: 127,
    };
    build_u32_batches(vec![data], d.path(), &cfg).unwrap();
    let manifest = add_exact_hierarchies(
        d.path(),
        &[
            HierarchySpec { columns: vec![0] },
            HierarchySpec { columns: vec![1] },
        ],
        113,
    )
    .unwrap();

    let low = manifest.hierarchies.iter().find(|h| h.columns == vec![0]).unwrap();
    let high = manifest.hierarchies.iter().find(|h| h.columns == vec![1]).unwrap();
    assert_eq!(low.kind, "bitslice");
    assert_ne!(high.kind, "bitslice");

    let engine = Engine::open(d.path()).unwrap();
    for source in [0usize, 1, 777, 12_345, rows - 1] {
        let low_value = (source % 8) as u64;
        let high_value = ((source * 997 + 31) % 100_000) as u64;
        let q = [
            Predicate { column: 0, value: low_value },
            Predicate { column: 1, value: high_value },
        ];
        let stats = engine.query(&q);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.rows_checked, 0);
        assert_eq!(stats.pages_touched, 0);
    }
}

#[test]
fn medium_cardinality_singletons_can_spend_small_storage_premium_for_bitslices() {
    let d = tempfile::tempdir().unwrap();
    let rows = 100_003usize;
    let cards = vec![8u64, 64u64, 100_000u64];
    let mut data = Vec::with_capacity(rows * 3);
    for r in 0..rows {
        data.push((r % 8) as u32);
        data.push(((r * 37 + r / 11 + 5) % 64) as u32);
        data.push(((r * 997 + 31) % 100_000) as u32);
    }

    let cfg = BuildConfig {
        columns: 3,
        page_rows: 257,
        cardinalities: cards,
        hierarchies: vec![],
        max_sort_records: 251,
    };
    build_u32_batches(vec![data], d.path(), &cfg).unwrap();
    let manifest = add_exact_hierarchies(
        d.path(),
        &[
            HierarchySpec { columns: vec![0] },
            HierarchySpec { columns: vec![1] },
            HierarchySpec { columns: vec![2] },
        ],
        239,
    )
    .unwrap();

    let low = manifest.hierarchies.iter().find(|h| h.columns == vec![0]).unwrap();
    let medium = manifest.hierarchies.iter().find(|h| h.columns == vec![1]).unwrap();
    let high = manifest.hierarchies.iter().find(|h| h.columns == vec![2]).unwrap();
    assert_eq!(low.kind, "bitslice");
    assert_eq!(medium.kind, "bitslice");
    assert_ne!(high.kind, "bitslice");

    let engine = Engine::open(d.path()).unwrap();
    for source in [0usize, 1, 777, 12_345, 99_999, rows - 1] {
        let q = [
            Predicate { column: 0, value: (source % 8) as u64 },
            Predicate { column: 1, value: ((source * 37 + source / 11 + 5) % 64) as u64 },
            Predicate { column: 2, value: ((source * 997 + 31) % 100_000) as u64 },
        ];
        let exact = engine.scan(&q);
        let stats = engine.query(&q);
        assert_eq!(stats.hits, exact.hits);
        assert_eq!(stats.rows_checked, 0);
        assert_eq!(stats.pages_touched, 0);
    }
}

#[test]
fn sparse_pair_keyspaces_use_flat_postings() {
    let d = tempfile::tempdir().unwrap();
    let rows = 50_003usize;
    let cards = vec![1_000_000u64, 1_000_000u64];
    let mut data = Vec::with_capacity(rows * 2);
    for r in 0..rows {
        data.push(((r * 7919 + 17) % 1_000_000) as u32);
        data.push(((r * 104729 + r / 7 + 31) % 1_000_000) as u32);
    }

    let cfg = BuildConfig {
        columns: 2,
        page_rows: 257,
        cardinalities: cards,
        hierarchies: vec![],
        max_sort_records: 127,
    };
    build_u32_batches(vec![data], d.path(), &cfg).unwrap();
    let manifest = add_exact_hierarchies(
        d.path(),
        &[HierarchySpec { columns: vec![0, 1] }],
        113,
    )
    .unwrap();

    assert_eq!(manifest.hierarchies.len(), 1);
    assert_eq!(manifest.hierarchies[0].kind, "flatpost");

    let engine = Engine::open(d.path()).unwrap();
    for source in [0usize, 1, 777, 12_345, 49_999, rows - 1] {
        let q = [
            Predicate { column: 0, value: ((source * 7919 + 17) % 1_000_000) as u64 },
            Predicate { column: 1, value: ((source * 104729 + source / 7 + 31) % 1_000_000) as u64 },
        ];
        let exact = engine.scan(&q);
        let stats = engine.query(&q);
        assert_eq!(stats.hits, exact.hits);
        assert_eq!(stats.rows_checked, 0);
        assert_eq!(stats.pages_touched, 0);
    }
}

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

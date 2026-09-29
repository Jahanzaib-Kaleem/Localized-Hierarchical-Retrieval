use lhr::{
    add_exact_hierarchies, build_hybrid_u32_batches, read_storage_layout, verify_dataset_structure,
    BuildConfig, Engine, HierarchySpec, Predicate, StorageMode,
};

#[test]
fn cold_parquet_columns_keep_exact_equality_off_canonical_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let rows = 4_096usize;
    let columns = 4usize;
    let mut tokens = Vec::with_capacity(rows * columns);
    for row in 0..rows {
        tokens.push((row % 4) as u32);       // hot low-cardinality
        tokens.push(row as u32);             // cold high-cardinality
        tokens.push((row % 8) as u32);       // hot low-cardinality
        tokens.push((row * 3) as u32);       // cold high-cardinality
    }

    let cfg = BuildConfig {
        columns,
        page_rows: 128,
        cardinalities: vec![4, rows as u64, 8, (rows * 3) as u64 + 1],
        hierarchies: Vec::new(),
        max_sort_records: 512,
    };
    build_hybrid_u32_batches([tokens], root, &cfg, &[1, 3], 256).unwrap();

    let singleton_specs = (0..columns)
        .map(|column| HierarchySpec {
            columns: vec![column],
        })
        .collect::<Vec<_>>();
    add_exact_hierarchies(root, &singleton_specs, 512).unwrap();

    let layout = read_storage_layout(root, columns, rows as u64).unwrap();
    assert_eq!(layout.mode, StorageMode::HybridParquet);
    assert_eq!(layout.hot_columns, vec![0, 2]);
    assert_eq!(layout.cold_columns, vec![1, 3]);
    assert!(!layout.parquet_payloads.is_empty());

    let engine = Engine::open(root).unwrap();
    let target = 1_337u64;
    let predicates = [
        Predicate {
            column: 1,
            value: target,
        },
        Predicate {
            column: 2,
            value: target % 8,
        },
    ];
    let stats = engine.query(&predicates);
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.rows_checked, 0, "cold predicate must stay on exact indexes");
    assert_eq!(stats.pages_touched, 0, "cold predicate must not touch canonical pages");

    let (ids, page_stats) = engine.query_row_ids(&predicates, 10);
    assert_eq!(ids, vec![target]);
    assert_eq!(page_stats.rows_checked, 0);
    assert_eq!(page_stats.pages_touched, 0);

    let projection = engine.row_projection(target, &[0, 1, 2, 3]).unwrap();
    assert_eq!(projection[0], target % 4);
    assert_eq!(projection[1], target);
    assert_eq!(projection[2], target % 8);
    assert_eq!(projection[3], target * 3);

    let verification = verify_dataset_structure(root).unwrap();
    assert!(verification.valid, "{:?}", verification.errors);
}

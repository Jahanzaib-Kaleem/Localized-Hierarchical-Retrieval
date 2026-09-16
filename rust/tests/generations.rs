use lhr::{
    add_exact_hierarchies, begin_generation, build_u32_batches, list_generations,
    publish_generation, resolve_dataset_root, rollback_generation, BuildConfig, HierarchySpec,
};

fn build_fixture(root: &std::path::Path, rows: usize, salt: usize) {
    let mut data = Vec::with_capacity(rows * 2);
    for r in 0..rows {
        data.push(((r + salt) % 8) as u32);
        data.push(((r * 31 + salt) % 257) as u32);
    }
    let cfg = BuildConfig {
        columns: 2,
        page_rows: 127,
        cardinalities: vec![8, 257],
        hierarchies: vec![],
        max_sort_records: 83,
    };
    build_u32_batches(vec![data], root, &cfg).unwrap();
    add_exact_hierarchies(
        root,
        &[
            HierarchySpec { columns: vec![0] },
            HierarchySpec { columns: vec![1] },
        ],
        83,
    )
    .unwrap();
}

#[test]
fn publish_switch_and_rollback_generations() {
    let catalog = tempfile::tempdir().unwrap();

    let first = begin_generation(catalog.path()).unwrap();
    assert_eq!(first.id, 1);
    build_fixture(&first.path, 1_003, 1);
    let g1 = publish_generation(first).unwrap();
    assert_eq!(g1.id, 1);
    assert!(g1.current);
    assert!(g1.sealed);
    assert_eq!(resolve_dataset_root(catalog.path()).unwrap(), g1.path);

    let second = begin_generation(catalog.path()).unwrap();
    assert_eq!(second.id, 2);
    build_fixture(&second.path, 2_003, 7);
    let g2 = publish_generation(second).unwrap();
    assert_eq!(resolve_dataset_root(catalog.path()).unwrap(), g2.path);

    let generations = list_generations(catalog.path()).unwrap();
    assert_eq!(generations.len(), 2);
    assert!(!generations[0].current);
    assert!(generations[1].current);

    let rolled = rollback_generation(catalog.path(), 1).unwrap();
    assert_eq!(rolled.id, 1);
    assert_eq!(resolve_dataset_root(catalog.path()).unwrap(), g1.path);
    let generations = list_generations(catalog.path()).unwrap();
    assert!(generations[0].current);
    assert!(!generations[1].current);
}

#[test]
fn legacy_dataset_roots_still_resolve_directly() {
    let legacy = tempfile::tempdir().unwrap();
    build_fixture(legacy.path(), 503, 3);
    assert_eq!(resolve_dataset_root(legacy.path()).unwrap(), legacy.path());
    assert!(list_generations(legacy.path()).unwrap().is_empty());
}

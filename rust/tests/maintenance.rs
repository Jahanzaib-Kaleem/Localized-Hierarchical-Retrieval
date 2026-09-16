use lhr::{
    add_exact_hierarchies, backup_dataset, begin_generation, build_u32_batches, list_generations,
    publish_generation, resolve_dataset_root, restore_backup, rollback_generation,
    vacuum_generations, BuildConfig, HierarchySpec, Manifest,
};
use std::fs;

fn publish_fixture(catalog: &std::path::Path, rows: usize, salt: usize) -> lhr::GenerationInfo {
    let stage = begin_generation(catalog).unwrap();
    let mut data = Vec::with_capacity(rows * 2);
    for r in 0..rows {
        data.push(((r + salt) % 8) as u32);
        data.push(((r * 17 + salt) % 257) as u32);
    }
    let cfg = BuildConfig {
        columns: 2,
        page_rows: 64,
        cardinalities: vec![8, 257],
        hierarchies: vec![],
        max_sort_records: 64,
    };
    build_u32_batches(vec![data], &stage.path, &cfg).unwrap();
    add_exact_hierarchies(
        &stage.path,
        &[
            HierarchySpec { columns: vec![0] },
            HierarchySpec { columns: vec![1] },
        ],
        64,
    )
    .unwrap();
    publish_generation(stage).unwrap()
}

#[test]
fn vacuum_keeps_current_newest_and_explicitly_protected_generations() {
    let catalog = tempfile::tempdir().unwrap();
    let g1 = publish_fixture(catalog.path(), 101, 1);
    let g2 = publish_fixture(catalog.path(), 102, 2);
    let g3 = publish_fixture(catalog.path(), 103, 3);
    let g4 = publish_fixture(catalog.path(), 104, 4);
    rollback_generation(catalog.path(), g2.id).unwrap();

    let stale_generation = catalog.path().join("generations/.staging-orphan");
    let stale_work = catalog.path().join(".mutation-work-orphan");
    fs::create_dir_all(&stale_generation).unwrap();
    fs::create_dir_all(&stale_work).unwrap();
    fs::write(stale_generation.join("junk"), b"junk").unwrap();
    fs::write(stale_work.join("junk"), b"junk").unwrap();

    let report = vacuum_generations(catalog.path(), 1, &[g1.id]).unwrap();
    assert_eq!(report.deleted, vec![g3.id]);
    assert_eq!(report.kept, vec![g1.id, g2.id, g4.id]);
    assert_eq!(report.stale_paths_removed, 2);
    assert!(report.bytes_reclaimed > 0);
    assert_eq!(resolve_dataset_root(catalog.path()).unwrap(), g2.path);

    let remaining: Vec<_> = list_generations(catalog.path())
        .unwrap()
        .into_iter()
        .map(|x| x.id)
        .collect();
    assert_eq!(remaining, vec![g1.id, g2.id, g4.id]);
}

#[test]
fn verified_backup_restores_as_a_new_generation() {
    let catalog = tempfile::tempdir().unwrap();
    let backup_parent = tempfile::tempdir().unwrap();
    let backup = backup_parent.path().join("snapshot");

    let g1 = publish_fixture(catalog.path(), 333, 5);
    backup_dataset(&g1.path, &backup).unwrap();
    let _g2 = publish_fixture(catalog.path(), 777, 9);

    let restored = restore_backup(catalog.path(), &backup).unwrap();
    assert_eq!(restored.id, 3);
    assert_eq!(resolve_dataset_root(catalog.path()).unwrap(), restored.path);
    let manifest: Manifest = serde_json::from_slice(&fs::read(restored.path.join("manifest.json")).unwrap())
        .unwrap();
    assert_eq!(manifest.rows, 333);
    assert!(restored.sealed);
}

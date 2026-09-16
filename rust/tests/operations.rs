use lhr::{
    add_exact_hierarchies, backup_dataset, build_u32_batches, dataset_status, seal_dataset,
    verify_dataset, BuildConfig, HierarchySpec,
};
use std::{fs::OpenOptions, io::Write};

fn build_fixture(root: &std::path::Path) {
    let rows = 10_003usize;
    let mut data = Vec::with_capacity(rows * 3);
    for r in 0..rows {
        data.push((r % 8) as u32);
        data.push((r % 17) as u32);
        data.push(((r * 97 + 11) % 1000) as u32);
    }
    let cfg = BuildConfig {
        columns: 3,
        page_rows: 257,
        cardinalities: vec![8, 17, 1000],
        hierarchies: vec![],
        max_sort_records: 127,
    };
    build_u32_batches(vec![data], root, &cfg).unwrap();
    add_exact_hierarchies(
        root,
        &[
            HierarchySpec { columns: vec![0] },
            HierarchySpec { columns: vec![1] },
            HierarchySpec { columns: vec![2] },
            HierarchySpec { columns: vec![0, 1] },
        ],
        127,
    )
    .unwrap();
}

#[test]
fn seal_verify_and_backup_roundtrip() {
    let source = tempfile::tempdir().unwrap();
    build_fixture(source.path());

    let unsealed = verify_dataset(source.path()).unwrap();
    assert!(unsealed.valid);
    assert!(unsealed.warnings.iter().any(|x| x.contains("no integrity seal")));

    let seal = seal_dataset(source.path()).unwrap();
    assert!(!seal.entries.is_empty());
    let verified = verify_dataset(source.path()).unwrap();
    assert!(verified.valid, "{:?}", verified.errors);
    assert!(verified.warnings.is_empty());

    let status = dataset_status(source.path()).unwrap();
    assert_eq!(status.rows, 10_003);
    assert_eq!(status.columns, 3);
    assert_eq!(status.hierarchies, 4);
    assert!(status.integrity_present);
    assert!(status.total_bytes >= status.canonical_bytes + status.routing_bytes);

    let parent = tempfile::tempdir().unwrap();
    let destination = parent.path().join("snapshot");
    let copied = backup_dataset(source.path(), &destination).unwrap();
    assert!(copied.valid, "{:?}", copied.errors);
    assert!(destination.join("integrity.json").is_file());
    assert_eq!(dataset_status(&destination).unwrap().rows, 10_003);
}

#[test]
fn checksum_detects_tampering() {
    let source = tempfile::tempdir().unwrap();
    build_fixture(source.path());
    seal_dataset(source.path()).unwrap();

    let manifest: lhr::Manifest = serde_json::from_slice(
        &std::fs::read(source.path().join("manifest.json")).unwrap(),
    )
    .unwrap();
    let segment = source.path().join("canonical").join(&manifest.segments[0].file);
    let mut file = OpenOptions::new().append(true).open(segment).unwrap();
    file.write_all(&[0x55]).unwrap();
    file.sync_all().unwrap();

    let report = verify_dataset(source.path()).unwrap();
    assert!(!report.valid);
    assert!(report
        .errors
        .iter()
        .any(|x| x.contains("segment") || x.contains("checksum") || x.contains("size mismatch")));
}

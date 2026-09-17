use lhr::{
    import_csv, ColumnSchema, CsvImportConfig, DatasetSchema, LogicalPredicate, LogicalType,
    Normalization, ShardSpec, ShardedDataset,
};
use std::{fs, path::Path};

fn schema() -> DatasetSchema {
    DatasetSchema::new(vec![
        ColumnSchema {
            name: "id".into(),
            logical_type: LogicalType::Unsigned,
            nullable: false,
            normalization: Normalization::Trim,
            null_values: vec![],
        },
        ColumnSchema {
            name: "status".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: vec![],
        },
    ])
    .unwrap()
}

fn import_shard(root: &Path, start: u64, rows: u64) {
    let source = root.join("source.csv");
    let mut csv = String::from("id,status\n");
    for local in 0..rows {
        let status = if local % 2 == 0 { "active" } else { "inactive" };
        csv.push_str(&format!("{},{}\n", start + local, status));
    }
    fs::write(&source, csv).unwrap();
    import_csv(
        root,
        &source,
        &schema(),
        &CsvImportConfig {
            page_rows: 64,
            batch_rows: 128,
            max_sort_records: 2_048,
            dictionary_run_bytes: 16 * 1024,
            accelerators: vec![],
        },
    )
    .unwrap();
}

#[test]
fn immutable_shards_merge_into_one_exact_global_row_space() {
    let shard_a = tempfile::tempdir().unwrap();
    let shard_b = tempfile::tempdir().unwrap();
    import_shard(shard_a.path(), 0, 1_000);
    import_shard(shard_b.path(), 1_000, 1_000);

    let dataset = ShardedDataset::open(vec![
        ShardSpec { root: shard_b.path().to_path_buf(), row_base: 1_000_000 },
        ShardSpec { root: shard_a.path().to_path_buf(), row_base: 0 },
    ])
    .unwrap();
    assert_eq!(dataset.shard_count(), 2);
    assert_eq!(dataset.physical_rows(), 2_000);
    assert_eq!(dataset.max_row_id(), Some(1_000_999));

    let predicate = [LogicalPredicate {
        column: "status".into(),
        value: Some("active".into()),
    }];
    let first = dataset
        .query_values_after(&predicate, Some(&["id".into()]), Some(994), 5)
        .unwrap();
    assert_eq!(first.hits, 1_000);
    assert_eq!(first.rows_checked, 0);
    assert_eq!(first.returned, 5);
    assert_eq!(
        first.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![996, 998, 1_000_000, 1_000_002, 1_000_004]
    );

    let second = dataset
        .query_values_after(&predicate, None, Some(1_000_994), 10)
        .unwrap();
    assert_eq!(second.hits, 1_000);
    assert_eq!(second.rows_checked, 0);
    assert_eq!(
        second.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![1_000_996, 1_000_998]
    );
}

#[test]
fn overlapping_global_shard_ranges_are_rejected() {
    let shard_a = tempfile::tempdir().unwrap();
    let shard_b = tempfile::tempdir().unwrap();
    import_shard(shard_a.path(), 0, 10);
    import_shard(shard_b.path(), 10, 10);

    let error = ShardedDataset::open(vec![
        ShardSpec { root: shard_a.path().to_path_buf(), row_base: 0 },
        ShardSpec { root: shard_b.path().to_path_buf(), row_base: 5 },
    ])
    .err()
    .expect("overlapping shard ranges should be rejected");
    assert!(error.to_string().contains("overlap"));
}

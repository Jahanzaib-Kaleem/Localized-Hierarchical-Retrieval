use lhr::{
    bucket_root, combine_buckets, create_bucket, dataset_stats, import_csv, list_buckets,
    rename_bucket, transfer_rows, ColumnSchema, CsvImportConfig, DatasetSchema, LogicalType,
    MutationConfig, Normalization,
};
use std::fs;

fn schema() -> DatasetSchema {
    DatasetSchema::new(vec![
        ColumnSchema {
            name: "email".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: vec![],
        },
        ColumnSchema {
            name: "country".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: vec![],
        },
    ])
    .unwrap()
}

fn import_config() -> CsvImportConfig {
    CsvImportConfig {
        page_rows: 16,
        batch_rows: 32,
        max_sort_records: 128,
        dictionary_run_bytes: 4096,
        accelerators: vec![],
    }
}

fn mutation_config() -> MutationConfig {
    MutationConfig {
        batch_rows: 16,
        max_sort_records: 128,
        dictionary_run_bytes: 4096,
    }
}

#[test]
fn legacy_default_and_named_bucket_workflows_are_compatible() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("default.csv");
    fs::write(
        &source,
        "email,country\na@example.com,pk\nb@example.com,us\n",
    )
    .unwrap();
    import_csv(root.path(), &source, &schema(), &import_config()).unwrap();

    let initial = list_buckets(root.path()).unwrap();
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].id, "default");
    assert!(initial[0].ready);
    assert_eq!(initial[0].rows, 2);

    let renamed = rename_bucket(root.path(), "default", "Shopify master").unwrap();
    assert_eq!(renamed.name, "Shopify master");
    assert_eq!(list_buckets(root.path()).unwrap()[0].name, "Shopify master");

    create_bucket(root.path(), "qualified", "Qualified").unwrap();
    let qualified_root = bucket_root(root.path(), "qualified").unwrap();
    let qualified_seed = root.path().join("qualified.csv");
    fs::write(&qualified_seed, "email,country\nseed@example.com,pk\n").unwrap();
    import_csv(&qualified_root, &qualified_seed, &schema(), &import_config()).unwrap();

    let copy = transfer_rows(
        root.path(),
        "default",
        "qualified",
        &[0],
        false,
        &mutation_config(),
    )
    .unwrap();
    assert_eq!(copy.rows_requested, 1);
    assert_eq!(dataset_stats(&qualified_root).unwrap().rows, 2);
    assert_eq!(dataset_stats(root.path()).unwrap().rows, 2);

    let moved = transfer_rows(
        root.path(),
        "default",
        "qualified",
        &[1],
        true,
        &mutation_config(),
    )
    .unwrap();
    assert!(moved.warning.is_none());
    assert_eq!(dataset_stats(root.path()).unwrap().rows, 1);
    assert_eq!(dataset_stats(&qualified_root).unwrap().rows, 3);

    let combined = combine_buckets(
        root.path(),
        &["default".into(), "qualified".into()],
        "combined",
        "Combined",
        &import_config(),
    )
    .unwrap();
    assert_eq!(combined.rows, 4);
    assert_eq!(dataset_stats(bucket_root(root.path(), "combined").unwrap()).unwrap().rows, 4);
}

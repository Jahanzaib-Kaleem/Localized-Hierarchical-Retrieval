use lhr::{
    add_index, dataset_stats, drop_index, import_csv, list_indexes, rebuild_index, verify_dataset,
    ColumnSchema, CsvImportConfig, DatasetSchema, LogicalDataset, LogicalPredicate, LogicalType,
    Normalization,
};
use std::{collections::BTreeSet, fs};

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

fn seed(catalog: &std::path::Path) {
    let csv = catalog.join("seed.csv");
    fs::write(
        &csv,
        "email,country,status\na@example.com,pk,new\nb@example.com,us,new\nc@example.com,pk,won\nd@example.com,gb,lost\n",
    )
    .unwrap();
    import_csv(
        catalog,
        &csv,
        &schema(),
        &CsvImportConfig {
            page_rows: 2,
            batch_rows: 2,
            max_sort_records: 32,
            dictionary_run_bytes: 64,
            accelerators: vec![vec![0, 1]],
        },
    )
    .unwrap();
}

fn pair_names() -> BTreeSet<Vec<String>> {
    BTreeSet::from([
        vec!["country".to_string(), "email".to_string()],
        vec!["country".to_string(), "status".to_string()],
    ])
}

#[test]
fn stats_and_index_lifecycle_are_generation_safe() {
    let catalog = tempfile::tempdir().unwrap();
    seed(catalog.path());

    let stats = dataset_stats(catalog.path()).unwrap();
    assert_eq!(stats.rows, 4);
    assert_eq!(stats.columns, 3);
    assert_eq!(stats.column_stats[0].cardinality, 4);
    assert!(stats.column_stats.iter().all(|x| x.dictionary_bytes > 0));
    assert!(stats.indexes.iter().all(|x| x.bytes > 0));
    assert!(stats
        .indexes
        .iter()
        .any(|x| x.column_names == vec!["email", "country"]));

    let add = add_index(
        catalog.path(),
        &["country".into(), "status".into()],
        32,
    )
    .unwrap();
    assert_eq!(add.generation.id, 2);
    assert!(verify_dataset(&add.generation.path).unwrap().valid);
    assert!(add
        .indexes
        .iter()
        .any(|x| x.column_names == vec!["country", "status"]));

    let before_drop_files: BTreeSet<_> = list_indexes(catalog.path())
        .unwrap()
        .into_iter()
        .map(|x| x.file)
        .collect();
    let drop = drop_index(
        catalog.path(),
        &["email".into(), "country".into()],
    )
    .unwrap();
    assert_eq!(drop.generation.id, 3);
    assert!(!drop
        .indexes
        .iter()
        .any(|x| x.column_names == vec!["email", "country"]));
    // Correctness singletons are deliberately non-droppable through this API.
    assert_eq!(
        drop_index(catalog.path(), &["email".into()]).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );

    // Add another accelerator after a manifest hole. New hierarchy filenames must be allocated
    // above surviving IDs rather than overwriting one of them.
    let add_after_drop = add_index(
        catalog.path(),
        &["email".into(), "status".into()],
        32,
    )
    .unwrap();
    assert_eq!(add_after_drop.generation.id, 4);
    let after_files: BTreeSet<_> = add_after_drop
        .indexes
        .iter()
        .map(|x| x.file.clone())
        .collect();
    assert!(before_drop_files.intersection(&after_files).count() >= 3);
    assert_eq!(after_files.len(), add_after_drop.indexes.len());

    let rebuild = rebuild_index(
        catalog.path(),
        &["country".into(), "status".into()],
        32,
    )
    .unwrap();
    assert_eq!(rebuild.generation.id, 5);
    assert!(verify_dataset(&rebuild.generation.path).unwrap().valid);

    let pairs: BTreeSet<Vec<String>> = rebuild
        .indexes
        .iter()
        .filter(|x| x.exact_rows && x.columns.len() == 2)
        .map(|x| {
            let mut names = x.column_names.clone();
            names.sort();
            names
        })
        .collect();
    let mut expected = pair_names();
    expected.insert(vec!["email".into(), "status".into()]);
    // Original email+country was intentionally dropped, so only the new/rebuilt accelerators remain.
    expected.remove(&vec!["country".into(), "email".into()]);
    assert_eq!(pairs, expected);

    let logical = LogicalDataset::open(catalog.path()).unwrap();
    let result = logical
        .query_values(
            &[
                LogicalPredicate {
                    column: "country".into(),
                    value: Some("pk".into()),
                },
                LogicalPredicate {
                    column: "status".into(),
                    value: Some("won".into()),
                },
            ],
            Some(&["email".into()]),
            10,
        )
        .unwrap();
    assert_eq!(result.hits, 1);
    assert_eq!(result.rows[0].values[0].value.as_deref(), Some("c@example.com"));
}

use lhr::{
    import_csv, list_generations, resolve_dataset_root, verify_dataset, ColumnSchema, CsvImportConfig,
    DatasetSchema, LogicalDataset, LogicalPredicate, LogicalType, Normalization,
};
use std::fs;

fn schema() -> DatasetSchema {
    DatasetSchema::new(vec![
        ColumnSchema {
            name: "country".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: vec![],
        },
        ColumnSchema {
            name: "age".into(),
            logical_type: LogicalType::Unsigned,
            nullable: false,
            normalization: Normalization::Trim,
            null_values: vec![],
        },
        ColumnSchema {
            name: "active".into(),
            logical_type: LogicalType::Boolean,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: vec![],
        },
        ColumnSchema {
            name: "note".into(),
            logical_type: LogicalType::Text,
            nullable: true,
            normalization: Normalization::Trim,
            null_values: vec!["NULL".into()],
        },
    ])
    .unwrap()
}

fn config() -> CsvImportConfig {
    CsvImportConfig {
        page_rows: 2,
        batch_rows: 2,
        max_sort_records: 32,
        dictionary_run_bytes: 32,
        accelerators: vec![vec![0, 1]],
    }
}

#[test]
fn csv_import_publishes_verified_generation_and_named_queries() {
    let catalog = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let csv = source_dir.path().join("leads.csv");
    fs::write(
        &csv,
        "active,country,note,age\nTRUE, PK , hello ,042\n0,US,NULL,7\n1,pk,world,42\n",
    )
    .unwrap();

    let report = import_csv(catalog.path(), &csv, &schema(), &config()).unwrap();
    assert_eq!(report.rows, 3);
    assert_eq!(report.exact_hierarchies, 5); // four singletons + one accelerator
    assert_eq!(report.generation.id, 1);
    let resolved = resolve_dataset_root(catalog.path()).unwrap();
    assert_eq!(resolved, report.generation.path);
    assert!(verify_dataset(&resolved).unwrap().valid);

    let logical = LogicalDataset::open(catalog.path()).unwrap();
    let result = logical
        .query_values(
            &[LogicalPredicate {
                column: "country".into(),
                value: Some(" PK ".into()),
            }],
            Some(&["age".into(), "active".into(), "note".into()]),
            10,
        )
        .unwrap();
    assert_eq!(result.hits, 2);
    assert_eq!(result.returned, 2);
    assert_eq!(result.rows[0].values[0].value.as_deref(), Some("42"));
    assert_eq!(result.rows[0].values[1].value.as_deref(), Some("true"));
    assert_eq!(result.rows[0].values[2].value.as_deref(), Some("hello"));

    let null_result = logical
        .query_values(
            &[LogicalPredicate {
                column: "note".into(),
                value: None,
            }],
            Some(&["country".into()]),
            10,
        )
        .unwrap();
    assert_eq!(null_result.hits, 1);
    assert_eq!(null_result.rows[0].values[0].value.as_deref(), Some("us"));
}

#[test]
fn failed_import_does_not_move_current_generation() {
    let catalog = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let good = source_dir.path().join("good.csv");
    let bad = source_dir.path().join("bad.csv");
    fs::write(
        &good,
        "country,age,active,note\npk,20,true,ok\nus,30,false,NULL\n",
    )
    .unwrap();
    fs::write(
        &bad,
        "country,age,active,note\npk,not-a-number,true,broken\n",
    )
    .unwrap();

    let first = import_csv(catalog.path(), &good, &schema(), &config()).unwrap();
    let current_before = resolve_dataset_root(catalog.path()).unwrap();
    assert_eq!(current_before, first.generation.path);

    let error = import_csv(catalog.path(), &bad, &schema(), &config()).unwrap_err();
    assert!(error.to_string().contains("unsigned integer"));
    assert_eq!(resolve_dataset_root(catalog.path()).unwrap(), current_before);
    let generations = list_generations(catalog.path()).unwrap();
    assert_eq!(generations.len(), 1);
    assert!(generations[0].current);
}

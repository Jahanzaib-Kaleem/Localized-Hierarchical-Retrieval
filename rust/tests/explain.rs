use lhr::{
    import_csv, ColumnSchema, CsvImportConfig, DatasetSchema, LogicalDataset, LogicalPredicate,
    LogicalType, Normalization,
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

#[test]
fn explain_reports_direct_and_composed_exact_routes() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("rows.csv");
    fs::write(
        &source,
        "email,country,status\na@example.com,pk,new\nb@example.com,pk,won\nc@example.com,us,new\nd@example.com,gb,lost\n",
    )
    .unwrap();
    import_csv(
        catalog.path(),
        &source,
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

    let dataset = LogicalDataset::open(catalog.path()).unwrap();
    let direct = dataset
        .explain_values(&[
            LogicalPredicate {
                column: "email".into(),
                value: Some("A@EXAMPLE.COM".into()),
            },
            LogicalPredicate {
                column: "country".into(),
                value: Some("pk".into()),
            },
        ])
        .unwrap();
    assert!(!direct.dictionary_miss);
    let direct = direct.plan.unwrap();
    assert_eq!(direct.route, "exact_rows");
    assert!(direct.fully_covered);
    assert!(direct.exact_result_proven);
    assert!(!direct.canonical_verification_required);
    assert_eq!(direct.candidate_rows, Some(1));
    assert_eq!(direct.selected_indexes.len(), 1);
    assert_eq!(direct.selected_indexes[0].columns, vec![0, 1]);

    let composed = dataset
        .explain_values(&[
            LogicalPredicate {
                column: "country".into(),
                value: Some("pk".into()),
            },
            LogicalPredicate {
                column: "status".into(),
                value: Some("new".into()),
            },
        ])
        .unwrap()
        .plan
        .unwrap();
    assert_eq!(composed.route, "exact_rows");
    assert_eq!(composed.candidate_rows, Some(1));
    assert_eq!(composed.selected_indexes.len(), 2);
    assert!(composed.selected_indexes.iter().all(|x| x.columns.len() == 1));

    let miss = dataset
        .explain_values(&[LogicalPredicate {
            column: "email".into(),
            value: Some("missing@example.com".into()),
        }])
        .unwrap();
    assert!(miss.dictionary_miss);
    assert!(miss.plan.is_none());
}

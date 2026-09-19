use lhr::{
    append_csv_delta, import_csv, list_generations, ColumnSchema, CsvImportConfig, DatasetSchema,
    LogicalPredicate, LogicalType, Normalization, VersionedDataset,
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
            name: "visits".into(),
            logical_type: LogicalType::Unsigned,
            nullable: false,
            normalization: Normalization::Trim,
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
        page_rows: 16,
        batch_rows: 32,
        max_sort_records: 128,
        dictionary_run_bytes: 4096,
        accelerators: vec![],
    }
}

#[test]
fn append_csv_adds_an_indexed_delta_without_rewriting_existing_rows() {
    let catalog = tempfile::tempdir().unwrap();
    let first = catalog.path().join("first.csv");
    let second = catalog.path().join("second.csv");
    fs::write(
        &first,
        "email,visits,note\na@example.com,10,one\nb@example.com,20,NULL\nc@example.com,30,three\n",
    )
    .unwrap();
    fs::write(
        &second,
        "note,email,visits\nfour,D@EXAMPLE.COM,40\nNULL,e@example.com,50\n",
    )
    .unwrap();

    import_csv(catalog.path(), &first, &schema(), &config()).unwrap();
    let report = append_csv_delta(catalog.path(), &second, &schema(), &config()).unwrap();
    assert_eq!(report.rows_before, 3);
    assert_eq!(report.appended, 2);
    assert_eq!(report.rows_after, 5);
    assert_eq!(report.max_row_id, Some(4));

    let view = VersionedDataset::open(catalog.path()).unwrap();
    assert_eq!(view.visible_rows(), 5);
    assert_eq!(view.row_values(0).unwrap().unwrap()[0].as_deref(), Some("a@example.com"));
    assert_eq!(view.row_values(3).unwrap().unwrap()[0].as_deref(), Some("d@example.com"));
    assert_eq!(view.row_values(4).unwrap().unwrap()[2], None);

    let result = view
        .query_values(
            &[LogicalPredicate {
                column: "email".into(),
                value: Some("D@EXAMPLE.COM".into()),
            }],
            None,
            10,
        )
        .unwrap();
    assert_eq!(result.hits, 1);
    assert_eq!(result.rows[0].row_id, 3);
}

#[test]
fn incompatible_append_is_rejected_and_current_generation_is_unchanged() {
    let catalog = tempfile::tempdir().unwrap();
    let first = catalog.path().join("first.csv");
    let bad = catalog.path().join("bad.csv");
    fs::write(&first, "email,visits,note\na@example.com,10,one\n").unwrap();
    fs::write(&bad, "email,visits,note\nb@example.com,20,two\n").unwrap();
    import_csv(catalog.path(), &first, &schema(), &config()).unwrap();

    let before = list_generations(catalog.path()).unwrap();
    let mut incompatible = schema();
    incompatible.columns[1].logical_type = LogicalType::Text;
    let error = append_csv_delta(catalog.path(), &bad, &incompatible, &config()).unwrap_err();
    assert!(error.to_string().contains("append schema mismatch"));

    let after = list_generations(catalog.path()).unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(
        after.iter().find(|generation| generation.current).unwrap().id,
        before.iter().find(|generation| generation.current).unwrap().id
    );
    assert_eq!(VersionedDataset::open(catalog.path()).unwrap().visible_rows(), 1);
}

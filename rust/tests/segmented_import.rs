use lhr::{
    append_csv_segmented_with_progress, import_csv, import_csv_segmented_initial_with_progress,
    list_generations, verify_versioned_dataset, ColumnSchema, CsvImportConfig, DatasetSchema,
    LogicalPredicate, LogicalType, Normalization, SegmentedCsvImportConfig, VersionedDataset,
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
            name: "segment".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: vec![],
        },
    ])
    .unwrap()
}

fn engine() -> CsvImportConfig {
    CsvImportConfig {
        page_rows: 2,
        batch_rows: 2,
        max_sort_records: 16,
        dictionary_run_bytes: 64,
        accelerators: vec![],
    }
}

fn segmented() -> SegmentedCsvImportConfig {
    let mut config = SegmentedCsvImportConfig::from_engine(engine());
    config.part_rows = 2;
    config.part_bytes = 1024 * 1024;
    config.reclaim_consumed_source = false;
    config
}

#[test]
fn segmented_create_publishes_one_generation_with_searchable_parts() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("bulk.csv");
    fs::write(
        &source,
        concat!(
            "email,visits,segment\n",
            "A@example.com,10,one\n",
            "b@example.com,20,two\n",
            "c@example.com,30,one\n",
            "d@example.com,40,two\n",
            "e@example.com,50,three\n",
        ),
    )
    .unwrap();

    let report = import_csv_segmented_initial_with_progress(
        catalog.path(),
        &source,
        &schema(),
        &segmented(),
        |_| {},
    )
    .unwrap();

    assert_eq!(report.rows, 5);
    assert_eq!(report.parts, 3);
    assert_eq!(list_generations(catalog.path()).unwrap().len(), 1);

    let view = VersionedDataset::open(catalog.path()).unwrap();
    assert_eq!(view.visible_rows(), 5);
    assert_eq!(view.delta_meta().len(), 2);
    assert_eq!(view.row_values(0).unwrap().unwrap()[0].as_deref(), Some("a@example.com"));
    assert_eq!(view.row_values(4).unwrap().unwrap()[0].as_deref(), Some("e@example.com"));

    let result = view
        .query_values(
            &[LogicalPredicate {
                column: "email".into(),
                value: Some("D@EXAMPLE.COM".into()),
            }],
            Some(&["email".into(), "visits".into()]),
            10,
        )
        .unwrap();
    assert_eq!(result.hits, 1);
    assert_eq!(result.rows[0].row_id, 3);
    assert_eq!(result.rows[0].values[1].value.as_deref(), Some("40"));
    assert!(verify_versioned_dataset(&report.generation.path).unwrap().valid);
}

#[test]
fn segmented_create_failure_never_publishes_partial_parts() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("bad.csv");
    fs::write(
        &source,
        concat!(
            "email,visits,segment\n",
            "a@example.com,10,one\n",
            "b@example.com,20,two\n",
            "c@example.com,not-a-number,one\n",
        ),
    )
    .unwrap();

    let error = import_csv_segmented_initial_with_progress(
        catalog.path(),
        &source,
        &schema(),
        &segmented(),
        |_| {},
    )
    .unwrap_err();

    assert!(error.to_string().contains("unsigned integer"));
    assert!(list_generations(catalog.path()).unwrap().is_empty());
}

#[test]
fn segmented_append_adds_multiple_parts_in_one_atomic_generation() {
    let catalog = tempfile::tempdir().unwrap();
    let base = catalog.path().join("base.csv");
    let append = catalog.path().join("append.csv");
    fs::write(
        &base,
        "email,visits,segment\na@example.com,10,one\nb@example.com,20,two\n",
    )
    .unwrap();
    fs::write(
        &append,
        concat!(
            "email,visits,segment\n",
            "c@example.com,30,one\n",
            "d@example.com,40,two\n",
            "e@example.com,50,three\n",
            "f@example.com,60,three\n",
            "g@example.com,70,four\n",
        ),
    )
    .unwrap();

    import_csv(catalog.path(), &base, &schema(), &engine()).unwrap();
    let before = list_generations(catalog.path()).unwrap();
    let report = append_csv_segmented_with_progress(
        catalog.path(),
        &append,
        &schema(),
        &segmented(),
        |_| {},
    )
    .unwrap();

    assert_eq!(report.rows_before, 2);
    assert_eq!(report.appended, 5);
    assert_eq!(report.rows_after, 7);
    assert_eq!(report.parts, 3);
    assert_eq!(report.max_row_id, Some(6));

    let after = list_generations(catalog.path()).unwrap();
    assert_eq!(after.len(), before.len() + 1);
    let view = VersionedDataset::open(catalog.path()).unwrap();
    assert_eq!(view.visible_rows(), 7);
    assert_eq!(view.delta_meta().len(), 3);
    assert_eq!(view.row_values(6).unwrap().unwrap()[0].as_deref(), Some("g@example.com"));
    assert!(verify_versioned_dataset(&report.generation.path).unwrap().valid);
}

#[test]
fn segmented_append_failure_preserves_previous_current_generation() {
    let catalog = tempfile::tempdir().unwrap();
    let base = catalog.path().join("base.csv");
    let append = catalog.path().join("bad-append.csv");
    fs::write(
        &base,
        "email,visits,segment\na@example.com,10,one\nb@example.com,20,two\n",
    )
    .unwrap();
    fs::write(
        &append,
        concat!(
            "email,visits,segment\n",
            "c@example.com,30,one\n",
            "d@example.com,40,two\n",
            "e@example.com,broken,three\n",
        ),
    )
    .unwrap();

    import_csv(catalog.path(), &base, &schema(), &engine()).unwrap();
    let before = list_generations(catalog.path()).unwrap();
    let current_before = before.iter().find(|x| x.current).unwrap().id;

    let error = append_csv_segmented_with_progress(
        catalog.path(),
        &append,
        &schema(),
        &segmented(),
        |_| {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("unsigned integer"));

    let after = list_generations(catalog.path()).unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(after.iter().find(|x| x.current).unwrap().id, current_before);
    assert_eq!(VersionedDataset::open(catalog.path()).unwrap().visible_rows(), 2);
}

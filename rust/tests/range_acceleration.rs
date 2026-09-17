use lhr::{
    execute_query, import_csv, ColumnSchema, CsvImportConfig, DatasetSchema, LogicalType,
    Normalization, QueryFilter, QueryRequest, VersionedDataset,
};
use std::{fs, io};

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
            name: "visits".into(),
            logical_type: LogicalType::Unsigned,
            nullable: false,
            normalization: Normalization::Trim,
            null_values: vec![],
        },
    ])
    .unwrap()
}

fn import_config() -> CsvImportConfig {
    CsvImportConfig {
        page_rows: 128,
        batch_rows: 256,
        max_sort_records: 4_096,
        dictionary_run_bytes: 16 * 1024,
        accelerators: vec![],
    }
}

fn build_dataset() -> tempfile::TempDir {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("range.csv");
    let mut csv = String::from("id,visits\n");
    for row in 0u64..2_000 {
        csv.push_str(&format!("{row},{}\n", row % 1_000));
    }
    fs::write(&source, csv).unwrap();
    import_csv(catalog.path(), &source, &schema(), &import_config()).unwrap();
    catalog
}

fn bounded_request(after_row_id: Option<u64>) -> QueryRequest {
    QueryRequest {
        filters: vec![QueryFilter::Range {
            column: "visits".into(),
            gte: Some("100".into()),
            lte: Some("105".into()),
        }],
        select: vec!["id".into(), "visits".into()],
        limit: 5,
        after_row_id,
        max_rows_examined: Some(10),
        timeout_ms: Some(5_000),
    }
}

#[test]
fn bounded_integer_range_reuses_exact_singletons_without_scanning_rows() {
    let catalog = build_dataset();
    let dataset = VersionedDataset::open(catalog.path()).unwrap();

    let first = execute_query(&dataset, &bounded_request(None)).unwrap();
    assert_eq!(first.stats.hits, 12);
    assert_eq!(first.stats.rows_examined, 0);
    assert_eq!(first.returned, 5);
    assert_eq!(first.next_cursor, Some(104));
    assert_eq!(
        first.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![100, 101, 102, 103, 104]
    );

    let second = execute_query(&dataset, &bounded_request(first.next_cursor)).unwrap();
    assert_eq!(second.stats.hits, 12);
    assert_eq!(second.stats.rows_examined, 0);
    assert_eq!(second.returned, 5);
    assert_eq!(second.next_cursor, Some(1103));
    assert_eq!(
        second.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![105, 1100, 1101, 1102, 1103]
    );

    let third = execute_query(&dataset, &bounded_request(second.next_cursor)).unwrap();
    assert_eq!(third.stats.hits, 12);
    assert_eq!(third.stats.rows_examined, 0);
    assert_eq!(third.returned, 2);
    assert_eq!(third.next_cursor, None);
    assert_eq!(
        third.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![1104, 1105]
    );
}

#[test]
fn wide_integer_range_keeps_exact_scan_fallback_and_resource_cap() {
    let catalog = build_dataset();
    let dataset = VersionedDataset::open(catalog.path()).unwrap();
    let request = QueryRequest {
        filters: vec![QueryFilter::Range {
            column: "visits".into(),
            gte: Some("100".into()),
            lte: Some("500".into()),
        }],
        select: vec!["id".into()],
        limit: 5,
        after_row_id: None,
        max_rows_examined: Some(10),
        timeout_ms: Some(5_000),
    };

    let error = execute_query(&dataset, &request).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::OutOfMemory);
    assert!(error.to_string().contains("row-examination limit"));
}

use lhr::{
    apply_mutations_delta, execute_query, import_csv, ColumnSchema, CsvImportConfig, DatasetSchema,
    LogicalType, Mutation, MutationConfig, Normalization, QueryFilter, QueryRequest,
    VersionedDataset,
};
use std::{collections::BTreeMap, fs};

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
        page_rows: 32,
        batch_rows: 64,
        max_sort_records: 1024,
        dictionary_run_bytes: 4096,
        accelerators: vec![],
    }
}

fn mutation_config() -> MutationConfig {
    MutationConfig {
        batch_rows: 16,
        max_sort_records: 128,
        dictionary_run_bytes: 1024,
    }
}

fn country_request(after_row_id: Option<u64>, limit: usize) -> QueryRequest {
    QueryRequest {
        filters: vec![QueryFilter::Eq {
            column: "country".into(),
            value: Some("pk".into()),
        }],
        select: vec!["email".into()],
        limit,
        after_row_id,
        max_rows_examined: Some(0),
        timeout_ms: Some(5_000),
    }
}

#[test]
fn equality_cursor_seeks_without_replaying_the_prefix() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("seed.csv");
    let mut csv = String::from("email,country\n");
    for row in 0..300 {
        csv.push_str(&format!("row{row}@example.com,pk\n"));
    }
    fs::write(&source, csv).unwrap();
    import_csv(catalog.path(), &source, &schema(), &import_config()).unwrap();

    let view = VersionedDataset::open(catalog.path()).unwrap();
    let page = execute_query(&view, &country_request(Some(199), 10)).unwrap();
    assert_eq!(page.stats.hits, 300);
    assert_eq!(page.stats.rows_examined, 0);
    assert!(page.stats.optimized_equality_route);
    assert_eq!(
        page.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        (200u64..210).collect::<Vec<_>>()
    );
    assert_eq!(page.next_cursor, Some(209));

    let tail = execute_query(&view, &country_request(Some(295), 10)).unwrap();
    assert_eq!(tail.stats.hits, 300);
    assert_eq!(
        tail.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![296, 297, 298, 299]
    );
    assert_eq!(tail.next_cursor, None);
}

#[test]
fn equality_cursor_preserves_visibility_across_delta_layers() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("seed.csv");
    fs::write(
        &source,
        "email,country\nrow0@example.com,pk\nrow1@example.com,pk\nrow2@example.com,pk\nrow3@example.com,pk\nrow4@example.com,pk\nrow5@example.com,pk\n",
    )
    .unwrap();
    import_csv(catalog.path(), &source, &schema(), &import_config()).unwrap();

    apply_mutations_delta(
        catalog.path(),
        &[
            Mutation::Delete { row_id: 2 },
            Mutation::Update {
                row_id: 4,
                values: BTreeMap::from([("country".into(), Some("us".into()))]),
            },
            Mutation::Insert {
                values: BTreeMap::from([
                    ("email".into(), Some("row6@example.com".into())),
                    ("country".into(), Some("pk".into())),
                ]),
            },
        ],
        &mutation_config(),
    )
    .unwrap();

    let view = VersionedDataset::open(catalog.path()).unwrap();
    let page = execute_query(&view, &country_request(Some(1), 10)).unwrap();
    assert_eq!(page.stats.hits, 5);
    assert_eq!(page.stats.rows_examined, 0);
    assert_eq!(
        page.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![3, 5, 6]
    );
}


#[test]
fn zero_filter_browse_pages_by_logical_cursor_and_skips_tombstones() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("seed.csv");
    fs::write(
        &source,
        "email,country\nrow0@example.com,pk\nrow1@example.com,pk\nrow2@example.com,pk\nrow3@example.com,pk\nrow4@example.com,pk\nrow5@example.com,pk\n",
    )
    .unwrap();
    import_csv(catalog.path(), &source, &schema(), &import_config()).unwrap();
    apply_mutations_delta(
        catalog.path(),
        &[Mutation::Delete { row_id: 2 }],
        &mutation_config(),
    )
    .unwrap();

    let view = VersionedDataset::open(catalog.path()).unwrap();
    let request = |after_row_id, limit| QueryRequest {
        filters: vec![],
        select: vec!["email".into()],
        limit,
        after_row_id,
        max_rows_examined: Some(100),
        timeout_ms: Some(5_000),
    };

    let first = execute_query(&view, &request(None, 3)).unwrap();
    assert_eq!(first.stats.hits, 5);
    assert_eq!(
        first.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![0, 1, 3]
    );
    assert_eq!(first.next_cursor, Some(3));

    let second = execute_query(&view, &request(first.next_cursor, 3)).unwrap();
    assert_eq!(
        second.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert_eq!(second.next_cursor, None);
    assert!(second.stats.rows_examined <= 2);
}

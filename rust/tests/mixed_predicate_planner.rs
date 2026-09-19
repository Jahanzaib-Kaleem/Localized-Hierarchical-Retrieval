use lhr::{
    append_csv_delta, execute_query, import_csv, ColumnSchema, CsvImportConfig, DatasetSchema, LogicalType,
    Normalization, QueryFilter, QueryRequest, VersionedDataset,
};
use std::fs;

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
            name: "group".into(),
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
    ])
    .unwrap()
}

#[test]
fn mixed_equality_and_range_uses_bounded_equality_candidates() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("mixed.csv");
    let mut csv = String::from("id,group,visits\n");
    let mut expected = Vec::new();
    for row in 0u64..2_000 {
        let group = if row % 20 == 0 { "target" } else { "other" };
        let visits = row % 1_000;
        if group == "target" && (200..=400).contains(&visits) {
            expected.push(row);
        }
        csv.push_str(&format!("{row},{group},{visits}\n"));
    }
    fs::write(&source, csv).unwrap();
    import_csv(
        catalog.path(),
        &source,
        &schema(),
        &CsvImportConfig {
            page_rows: 128,
            batch_rows: 256,
            max_sort_records: 4096,
            dictionary_run_bytes: 16 * 1024,
            accelerators: vec![],
        },
    )
    .unwrap();

    let dataset = VersionedDataset::open(catalog.path()).unwrap();
    let response = execute_query(
        &dataset,
        &QueryRequest {
            filters: vec![
                QueryFilter::Eq {
                    column: "group".into(),
                    value: Some("TARGET".into()),
                },
                QueryFilter::Range {
                    column: "visits".into(),
                    gte: Some("200".into()),
                    lte: Some("400".into()),
                },
            ],
            select: vec!["id".into()],
            limit: 100,
            after_row_id: None,
            max_rows_examined: Some(150),
            timeout_ms: Some(5_000),
        },
    )
    .unwrap();

    assert_eq!(response.stats.hits as usize, expected.len());
    assert_eq!(response.returned, expected.len());
    assert!(response.stats.optimized_equality_route);
    assert!(response.stats.rows_examined <= 100);
    assert_eq!(
        response.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        expected
    );
}


fn multi_schema() -> DatasetSchema {
    DatasetSchema::new(vec![
        ColumnSchema {
            name: "id".into(),
            logical_type: LogicalType::Unsigned,
            nullable: false,
            normalization: Normalization::Trim,
            null_values: vec![],
        },
        ColumnSchema {
            name: "group".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: vec![],
        },
        ColumnSchema {
            name: "region".into(),
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
    ])
    .unwrap()
}

#[test]
fn mixed_multi_equality_and_range_preserves_exact_hits_and_projection() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("mixed-multi.csv");
    let mut csv = String::from("id,group,region,visits\n");
    let mut expected = Vec::new();
    for row in 0u64..50_000 {
        let group = if row % 2 == 0 { "target" } else { "other" };
        let region = if row % 3 == 0 { "east" } else { "west" };
        let visits = row % 1_000;
        if group == "target" && region == "east" && (250..=260).contains(&visits) {
            expected.push(row);
        }
        csv.push_str(&format!("{row},{group},{region},{visits}\n"));
    }
    fs::write(&source, csv).unwrap();
    import_csv(
        catalog.path(),
        &source,
        &multi_schema(),
        &CsvImportConfig {
            page_rows: 128,
            batch_rows: 1_024,
            max_sort_records: 50_000,
            dictionary_run_bytes: 64 * 1024,
            accelerators: vec![],
        },
    )
    .unwrap();

    let dataset = VersionedDataset::open(catalog.path()).unwrap();
    let response = execute_query(
        &dataset,
        &QueryRequest {
            filters: vec![
                QueryFilter::Eq {
                    column: "group".into(),
                    value: Some("TARGET".into()),
                },
                QueryFilter::Eq {
                    column: "region".into(),
                    value: Some("EAST".into()),
                },
                QueryFilter::Range {
                    column: "visits".into(),
                    gte: Some("250".into()),
                    lte: Some("260".into()),
                },
            ],
            select: vec!["id".into()],
            limit: 25,
            after_row_id: None,
            max_rows_examined: Some(9_000),
            timeout_ms: Some(5_000),
        },
    )
    .unwrap();

    assert_eq!(response.stats.hits as usize, expected.len());
    assert_eq!(response.returned, expected.len().min(25));
    assert!(response.stats.optimized_equality_route);
    assert!(response.stats.rows_examined <= 8_334);
    assert_eq!(
        response.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        expected.into_iter().take(25).collect::<Vec<_>>()
    );
}

#[test]
fn mixed_candidate_stream_plans_each_layer_once() {
    let catalog = tempfile::tempdir().unwrap();
    let config = CsvImportConfig {
        page_rows: 128,
        batch_rows: 1_024,
        max_sort_records: 50_000,
        dictionary_run_bytes: 64 * 1024,
        accelerators: vec![],
    };

    for part in 0u64..3 {
        let source = catalog.path().join(format!("part-{part}.csv"));
        let start = part * 5_000;
        let mut csv = String::from("id,group,region,visits\n");
        for offset in 0u64..5_000 {
            let row = start + offset;
            csv.push_str(&format!("{row},target,east,{}\n", row % 1_000));
        }
        fs::write(&source, csv).unwrap();
        if part == 0 {
            import_csv(catalog.path(), &source, &multi_schema(), &config).unwrap();
        } else {
            append_csv_delta(catalog.path(), &source, &multi_schema(), &config).unwrap();
        }
    }

    let dataset = VersionedDataset::open(catalog.path()).unwrap();
    let response = execute_query(
        &dataset,
        &QueryRequest {
            filters: vec![
                QueryFilter::Eq {
                    column: "group".into(),
                    value: Some("TARGET".into()),
                },
                QueryFilter::Range {
                    column: "visits".into(),
                    gte: Some("990".into()),
                    lte: Some("999".into()),
                },
            ],
            select: vec!["id".into()],
            limit: 25,
            after_row_id: None,
            max_rows_examined: Some(15_000),
            timeout_ms: Some(5_000),
        },
    )
    .unwrap();

    assert_eq!(response.stats.hits, 150);
    assert_eq!(response.returned, 25);
    assert_eq!(response.stats.rows_examined, 15_000);
    assert_eq!(response.stats.hierarchy_lookups, 3);
    assert_eq!(
        response.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        (990u64..1_000)
            .chain(1_990..2_000)
            .chain(2_990..2_995)
            .collect::<Vec<_>>()
    );

    let exact = execute_query(
        &dataset,
        &QueryRequest {
            filters: vec![
                QueryFilter::Eq {
                    column: "group".into(),
                    value: Some("TARGET".into()),
                },
                QueryFilter::Eq {
                    column: "region".into(),
                    value: Some("EAST".into()),
                },
            ],
            select: vec!["id".into()],
            limit: 25,
            after_row_id: None,
            max_rows_examined: Some(1),
            timeout_ms: Some(5_000),
        },
    )
    .unwrap();

    assert_eq!(exact.stats.hits, 15_000);
    assert_eq!(exact.returned, 25);
    assert_eq!(exact.stats.rows_examined, 0);
    assert_eq!(exact.stats.hierarchy_lookups, 6);
    assert_eq!(
        exact.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        (0u64..25).collect::<Vec<_>>()
    );
}

#[test]
fn mixed_stream_handles_null_from_schema_evolution_exactly() {
    let catalog = tempfile::tempdir().unwrap();
    let base_source = catalog.path().join("base-null-evolution.csv");
    let delta_source = catalog.path().join("delta-null-evolution.csv");

    let base_schema = DatasetSchema::new(vec![
        ColumnSchema {
            name: "id".into(),
            logical_type: LogicalType::Unsigned,
            nullable: false,
            normalization: Normalization::Trim,
            null_values: vec![],
        },
        ColumnSchema {
            name: "group".into(),
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
    ])
    .unwrap();
    let evolved_schema = DatasetSchema::new(vec![
        base_schema.columns[0].clone(),
        base_schema.columns[1].clone(),
        base_schema.columns[2].clone(),
        ColumnSchema {
            name: "tag".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: vec![],
        },
    ])
    .unwrap();
    let config = CsvImportConfig {
        page_rows: 32,
        batch_rows: 64,
        max_sort_records: 512,
        dictionary_run_bytes: 8 * 1024,
        accelerators: vec![],
    };

    let mut base_csv = String::from("id,group,visits\n");
    let mut delta_csv = String::from("id,group,visits,tag\n");
    for row in 0u64..100 {
        base_csv.push_str(&format!("{row},target,{row}\n"));
    }
    for row in 100u64..200 {
        delta_csv.push_str(&format!("{row},target,{},hot\n", row - 100));
    }
    fs::write(&base_source, base_csv).unwrap();
    fs::write(&delta_source, delta_csv).unwrap();
    import_csv(catalog.path(), &base_source, &base_schema, &config).unwrap();
    append_csv_delta(catalog.path(), &delta_source, &evolved_schema, &config).unwrap();

    let dataset = VersionedDataset::open(catalog.path()).unwrap();
    let response = execute_query(
        &dataset,
        &QueryRequest {
            filters: vec![
                QueryFilter::Eq {
                    column: "tag".into(),
                    value: None,
                },
                QueryFilter::Range {
                    column: "visits".into(),
                    gte: Some("10".into()),
                    lte: Some("19".into()),
                },
            ],
            select: vec!["id".into(), "tag".into()],
            limit: 50,
            after_row_id: None,
            max_rows_examined: Some(100),
            timeout_ms: Some(5_000),
        },
    )
    .unwrap();

    assert_eq!(response.stats.hits, 10);
    assert_eq!(response.stats.rows_examined, 100);
    assert_eq!(response.stats.hierarchy_lookups, 0);
    assert_eq!(
        response.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        (10u64..20).collect::<Vec<_>>()
    );
    assert!(response
        .rows
        .iter()
        .all(|row| row.values[1].column == "tag" && row.values[1].value.is_none()));
}

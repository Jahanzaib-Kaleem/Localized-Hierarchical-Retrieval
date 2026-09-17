use lhr::{
    import_csv, ColumnSchema, CsvImportConfig, DatasetSchema, LogicalDataset, LogicalPredicate,
    LogicalType, Manifest, Normalization,
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
fn broad_bitslice_singleton_seeks_a_deep_page_exactly() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("broad.csv");
    let mut csv = String::from("id,status\n");
    for row in 0u64..20_000 {
        let status = if row % 2 == 0 { "active" } else { "inactive" };
        csv.push_str(&format!("{row},{status}\n"));
    }
    fs::write(&source, csv).unwrap();

    let report = import_csv(
        catalog.path(),
        &source,
        &schema(),
        &CsvImportConfig {
            page_rows: 256,
            batch_rows: 1_024,
            max_sort_records: 20_000,
            dictionary_run_bytes: 64 * 1024,
            accelerators: vec![],
        },
    )
    .unwrap();

    let manifest: Manifest =
        serde_json::from_slice(&fs::read(report.generation.path.join("manifest.json")).unwrap())
            .unwrap();
    assert!(manifest
        .hierarchies
        .iter()
        .any(|hierarchy| hierarchy.columns == vec![1] && hierarchy.kind == "bitslice"));

    let dataset = LogicalDataset::open(catalog.path()).unwrap();
    let result = dataset
        .query_values_after(
            &[LogicalPredicate {
                column: "status".into(),
                value: Some("active".into()),
            }],
            Some(&["id".into()]),
            Some(18_000),
            5,
        )
        .unwrap();

    assert_eq!(result.hits, 10_000);
    assert_eq!(result.rows_checked, 0);
    assert_eq!(result.pages_touched, 0);
    assert_eq!(result.returned, 5);
    assert_eq!(
        result.rows.iter().map(|row| row.row_id).collect::<Vec<_>>(),
        vec![18_002, 18_004, 18_006, 18_008, 18_010]
    );
}

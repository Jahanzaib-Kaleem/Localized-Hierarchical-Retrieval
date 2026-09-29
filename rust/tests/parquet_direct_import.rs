use lhr::{
    compact_dataset, import_parquet_shards_initial, read_storage_layout, resolve_dataset_root,
    verify_dataset_structure, ColumnSchema, CompactionConfig, DatasetSchema, LogicalPredicate,
    LogicalType, Normalization, ParquetImportConfig, StorageMode, VersionedDataset,
};
use parquet::{
    data_type::{ByteArray, ByteArrayType, Int32Type},
    file::{properties::WriterProperties, writer::SerializedFileWriter},
    schema::parser::parse_message_type,
};
use std::{fs::File, path::Path, sync::Arc};

fn write_source(path: &Path, rows: &[(&str, &str, i32, &str)]) {
    let schema = Arc::new(
        parse_message_type(
            r#"
            message source {
              REQUIRED BYTE_ARRAY email (UTF8);
              REQUIRED BYTE_ARRAY country (UTF8);
              REQUIRED INT32 score;
              REQUIRED BYTE_ARRAY bio (UTF8);
            }
            "#,
        )
        .unwrap(),
    );
    let props = Arc::new(WriterProperties::builder().build());
    let mut writer = SerializedFileWriter::new(File::create(path).unwrap(), schema, props).unwrap();
    let mut row_group = writer.next_row_group().unwrap();

    let emails = rows
        .iter()
        .map(|row| ByteArray::from(row.0))
        .collect::<Vec<_>>();
    let countries = rows
        .iter()
        .map(|row| ByteArray::from(row.1))
        .collect::<Vec<_>>();
    let scores = rows.iter().map(|row| row.2).collect::<Vec<_>>();
    let bios = rows
        .iter()
        .map(|row| ByteArray::from(row.3))
        .collect::<Vec<_>>();

    let mut column = row_group.next_column().unwrap().unwrap();
    assert_eq!(
        column
            .typed::<ByteArrayType>()
            .write_batch(&emails, None, None)
            .unwrap(),
        rows.len()
    );
    column.close().unwrap();

    let mut column = row_group.next_column().unwrap().unwrap();
    assert_eq!(
        column
            .typed::<ByteArrayType>()
            .write_batch(&countries, None, None)
            .unwrap(),
        rows.len()
    );
    column.close().unwrap();

    let mut column = row_group.next_column().unwrap().unwrap();
    assert_eq!(
        column
            .typed::<Int32Type>()
            .write_batch(&scores, None, None)
            .unwrap(),
        rows.len()
    );
    column.close().unwrap();

    let mut column = row_group.next_column().unwrap().unwrap();
    assert_eq!(
        column
            .typed::<ByteArrayType>()
            .write_batch(&bios, None, None)
            .unwrap(),
        rows.len()
    );
    column.close().unwrap();
    assert!(row_group.next_column().unwrap().is_none());
    row_group.close().unwrap();
    writer.close().unwrap();
}

fn schema() -> DatasetSchema {
    DatasetSchema::new(vec![
        ColumnSchema {
            name: "email".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: Vec::new(),
        },
        ColumnSchema {
            name: "country".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::Trim,
            null_values: Vec::new(),
        },
        ColumnSchema {
            name: "score".into(),
            logical_type: LogicalType::Signed,
            nullable: false,
            normalization: Normalization::Trim,
            null_values: Vec::new(),
        },
        ColumnSchema {
            name: "bio".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::None,
            null_values: Vec::new(),
        },
    ])
    .unwrap()
}

#[test]
fn direct_parquet_shards_import_without_csv_and_compaction_stays_hybrid() {
    let dir = tempfile::tempdir().unwrap();
    let source_a = dir.path().join("apollo_data_0000.parquet");
    let source_b = dir.path().join("apollo_data_0001.parquet");
    write_source(
        &source_a,
        &[
            ("A@EXAMPLE.COM", "US", 10, "long repeated profile text"),
            ("b@example.com", "US", 20, "long repeated profile text"),
        ],
    );
    write_source(
        &source_b,
        &[
            ("c@example.com", "CA", 30, "long repeated profile text"),
            ("d@example.com", "US", 40, "long repeated profile text"),
        ],
    );

    let catalog = dir.path().join("catalog");
    let config = ParquetImportConfig {
        page_rows: 2,
        batch_rows: 2,
        max_sort_records: 16,
        dictionary_run_bytes: 1024,
        accelerators: vec![vec![1, 2]],
        cold_columns: vec![2, 3],
        row_group_rows: 2,
    };
    let report = import_parquet_shards_initial(
        &catalog,
        &[source_b.clone(), source_a.clone()],
        &schema(),
        &config,
    )
    .unwrap();
    assert_eq!(report.source_files, 2);
    assert_eq!(report.rows, 4);
    assert_eq!(report.hot_columns, vec![0, 1]);
    assert_eq!(report.cold_columns, vec![2, 3]);
    assert!(source_a.is_file() && source_b.is_file(), "sources must remain untouched");

    let generation = resolve_dataset_root(&catalog).unwrap();
    let layout = read_storage_layout(&generation, 4, 4).unwrap();
    assert_eq!(layout.mode, StorageMode::HybridParquet);
    assert_eq!(layout.hot_columns, vec![0, 1]);
    assert_eq!(layout.cold_columns, vec![2, 3]);
    assert!(generation.join("canonical/segment-000000.parquet").is_file());
    assert!(verify_dataset_structure(&generation).unwrap().valid);

    let dataset = VersionedDataset::open(&catalog).unwrap();
    let result = dataset
        .query_values(
            &[LogicalPredicate {
                column: "score".into(),
                value: Some("20".into()),
            }],
            Some(&["email".into(), "score".into(), "bio".into()]),
            10,
        )
        .unwrap();
    assert_eq!(result.hits, 1);
    assert_eq!(result.rows_checked, 0, "cold equality must remain index-only");
    assert_eq!(result.pages_touched, 0, "cold equality must not scan canonical pages");
    assert_eq!(result.rows[0].values[0].value.as_deref(), Some("b@example.com"));
    drop(dataset);

    let compacted = compact_dataset(
        &catalog,
        &CompactionConfig {
            batch_rows: 2,
            max_sort_records: 16,
            dictionary_run_bytes: 1024,
        },
    )
    .unwrap();
    assert_eq!(compacted.rows, 4);
    let layout = read_storage_layout(&compacted.generation.path, 4, 4).unwrap();
    assert_eq!(layout.mode, StorageMode::HybridParquet);
    assert_eq!(layout.hot_columns, vec![0, 1]);
    assert_eq!(layout.cold_columns, vec![2, 3]);
    assert!(verify_dataset_structure(&compacted.generation.path).unwrap().valid);

    let dataset = VersionedDataset::open(&catalog).unwrap();
    let result = dataset
        .query_values(
            &[LogicalPredicate {
                column: "score".into(),
                value: Some("20".into()),
            }],
            None,
            10,
        )
        .unwrap();
    assert_eq!(result.hits, 1);
    assert_eq!(result.rows_checked, 0);
    assert_eq!(result.pages_touched, 0);
}

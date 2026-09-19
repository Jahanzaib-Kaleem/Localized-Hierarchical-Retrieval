use lhr::{
    append_csv_delta, apply_mutations_delta, compact_dataset, import_csv, resolve_dataset_root,
    verify_versioned_dataset, ColumnSchema, CompactionConfig, CsvImportConfig, DatasetSchema,
    LogicalPredicate, LogicalType, Mutation, MutationConfig, Normalization, VersionedDataset,
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

fn import_config() -> CsvImportConfig {
    CsvImportConfig {
        page_rows: 2,
        batch_rows: 2,
        max_sort_records: 32,
        dictionary_run_bytes: 64,
        accelerators: vec![vec![0, 1]],
    }
}

fn mutation_config() -> MutationConfig {
    MutationConfig {
        batch_rows: 2,
        max_sort_records: 32,
        dictionary_run_bytes: 64,
    }
}

fn query_email(dataset: &VersionedDataset, email: &str) -> lhr::LogicalQueryResult {
    dataset
        .query_values(
            &[LogicalPredicate {
                column: "email".into(),
                value: Some(email.into()),
            }],
            None,
            10,
        )
        .unwrap()
}

#[test]
fn deltas_keep_base_immutable_and_compaction_preserves_results() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("seed.csv");
    fs::write(
        &source,
        "email,country,note\na@example.com,pk,alpha\nb@example.com,us,NULL\nc@example.com,pk,charlie\n",
    )
    .unwrap();
    let seed = import_csv(catalog.path(), &source, &schema(), &import_config()).unwrap();
    let seed_manifest = fs::read(seed.generation.path.join("manifest.json")).unwrap();

    let mutations = vec![
        Mutation::Update {
            row_id: 0,
            values: BTreeMap::from([("country".into(), Some("US".into()))]),
        },
        Mutation::Delete { row_id: 1 },
        Mutation::Insert {
            values: BTreeMap::from([
                ("email".into(), Some("D@EXAMPLE.COM".into())),
                ("country".into(), Some("pk".into())),
                ("note".into(), Some("NULL".into())),
            ]),
        },
    ];
    let delta = apply_mutations_delta(catalog.path(), &mutations, &mutation_config()).unwrap();
    assert_eq!(delta.rows_after, 3);
    assert_eq!(delta.max_row_id, Some(3));
    assert!(delta.generation.path.join("overlay.json").is_file());
    assert!(delta.generation.path.join("visibility.bin").is_file());
    assert_eq!(
        fs::read(delta.generation.path.join("manifest.json")).unwrap(),
        seed_manifest,
        "base physical manifest must remain unchanged by a delta transaction"
    );
    assert!(verify_versioned_dataset(&delta.generation.path).unwrap().valid);

    let view = VersionedDataset::open(catalog.path()).unwrap();
    let a = query_email(&view, "a@example.com");
    assert_eq!(a.hits, 1);
    assert_eq!(a.rows[0].row_id, 0);
    assert_eq!(
        a.rows[0].values.iter().find(|x| x.column == "country").unwrap().value.as_deref(),
        Some("us")
    );
    assert_eq!(query_email(&view, "b@example.com").hits, 0);
    assert_eq!(query_email(&view, "c@example.com").rows[0].row_id, 2);
    let d = query_email(&view, "d@example.com");
    assert_eq!(d.rows[0].row_id, 3);
    assert_eq!(
        d.rows[0].values.iter().find(|x| x.column == "note").unwrap().value.as_deref(),
        Some("NULL")
    );
    drop(view);

    let compact = compact_dataset(
        catalog.path(),
        &CompactionConfig {
            batch_rows: 2,
            max_sort_records: 32,
            dictionary_run_bytes: 64,
        },
    )
    .unwrap();
    assert_eq!(compact.rows, 3);
    assert_eq!(compact.delta_layers_before, 1);
    assert!(!compact.generation.path.join("overlay.json").exists());
    assert!(!compact.generation.path.join("visibility.bin").exists());
    assert!(verify_versioned_dataset(&compact.generation.path).unwrap().valid);

    let compacted = VersionedDataset::open(catalog.path()).unwrap();
    assert_eq!(query_email(&compacted, "a@example.com").rows[0].row_id, 0);
    assert_eq!(query_email(&compacted, "b@example.com").hits, 0);
    assert_eq!(query_email(&compacted, "d@example.com").rows[0].row_id, 3);
    assert_eq!(resolve_dataset_root(catalog.path()).unwrap(), compact.generation.path);
}

#[test]
fn compaction_materializes_an_evolved_append_schema() {
    let catalog = tempfile::tempdir().unwrap();
    let first = catalog.path().join("seed.csv");
    let second = catalog.path().join("evolved.csv");
    fs::write(
        &first,
        "email,country,note\na@example.com,pk,one\nb@example.com,us,NULL\n",
    )
    .unwrap();
    fs::write(&second, "email,score\nc@example.com,42\n").unwrap();
    import_csv(catalog.path(), &first, &schema(), &import_config()).unwrap();

    let incoming = DatasetSchema::new(vec![
        schema().columns[0].clone(),
        ColumnSchema {
            name: "score".into(),
            logical_type: LogicalType::Unsigned,
            nullable: false,
            normalization: Normalization::Trim,
            null_values: vec![],
        },
    ])
    .unwrap();
    append_csv_delta(catalog.path(), &second, &incoming, &import_config()).unwrap();

    let before = VersionedDataset::open(catalog.path()).unwrap();
    assert_eq!(before.schema().columns.len(), 4);
    assert_eq!(before.row_values(0).unwrap().unwrap()[3], None);
    assert_eq!(before.row_values(2).unwrap().unwrap()[3].as_deref(), Some("42"));
    drop(before);

    let report = compact_dataset(
        catalog.path(),
        &CompactionConfig {
            batch_rows: 2,
            max_sort_records: 32,
            dictionary_run_bytes: 64,
        },
    )
    .unwrap();
    assert_eq!(report.rows, 3);

    let compacted = VersionedDataset::open(catalog.path()).unwrap();
    assert!(compacted.delta_meta().is_empty());
    assert_eq!(
        compacted
            .schema()
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        vec!["email", "country", "note", "score"]
    );
    assert_eq!(compacted.row_values(0).unwrap().unwrap()[3], None);
    assert_eq!(compacted.row_values(2).unwrap().unwrap()[3].as_deref(), Some("42"));
    assert!(verify_versioned_dataset(catalog.path()).unwrap().valid);
}

#[test]
fn later_delta_supersedes_earlier_delta_version() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("seed.csv");
    fs::write(&source, "email,country,note\na@example.com,pk,one\n").unwrap();
    import_csv(catalog.path(), &source, &schema(), &import_config()).unwrap();

    apply_mutations_delta(
        catalog.path(),
        &[Mutation::Update {
            row_id: 0,
            values: BTreeMap::from([("country".into(), Some("us".into()))]),
        }],
        &mutation_config(),
    )
    .unwrap();
    apply_mutations_delta(
        catalog.path(),
        &[Mutation::Update {
            row_id: 0,
            values: BTreeMap::from([("country".into(), Some("ca".into()))]),
        }],
        &mutation_config(),
    )
    .unwrap();

    let view = VersionedDataset::open(catalog.path()).unwrap();
    let row = query_email(&view, "a@example.com");
    assert_eq!(row.hits, 1);
    assert_eq!(
        row.rows[0].values.iter().find(|x| x.column == "country").unwrap().value.as_deref(),
        Some("ca")
    );
}

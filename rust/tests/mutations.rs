use lhr::{
    apply_mutations, import_csv, list_generations, resolve_dataset_root, verify_dataset,
    ColumnSchema, CsvImportConfig, DatasetSchema, LogicalDataset, LogicalPredicate, LogicalType,
    Mutation, MutationConfig, Normalization,
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

fn query_email(dataset: &LogicalDataset, email: &str) -> lhr::LogicalQueryResult {
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
fn update_delete_insert_are_atomic_and_keep_stable_row_ids() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("seed.csv");
    fs::write(
        &source,
        "email,country,note\na@example.com,pk,alpha\nb@example.com,us,NULL\nc@example.com,pk,charlie\n",
    )
    .unwrap();
    import_csv(catalog.path(), &source, &schema(), &import_config()).unwrap();

    let before = LogicalDataset::open(catalog.path()).unwrap();
    assert_eq!(query_email(&before, "a@example.com").rows[0].row_id, 0);
    assert_eq!(query_email(&before, "b@example.com").rows[0].row_id, 1);
    drop(before);

    let mutations = vec![
        Mutation::Update {
            row_id: 0,
            values: BTreeMap::from([("country".into(), Some(" US ".into()))]),
        },
        Mutation::Delete { row_id: 1 },
        Mutation::Insert {
            values: BTreeMap::from([
                ("email".into(), Some("D@EXAMPLE.COM".into())),
                ("country".into(), Some("pk".into())),
                // This is intentionally a literal string even though the original CSV schema
                // treats the same spelling as a NULL literal.
                ("note".into(), Some("NULL".into())),
            ]),
        },
    ];
    let report = apply_mutations(catalog.path(), &mutations, &mutation_config()).unwrap();
    assert_eq!(report.rows_before, 3);
    assert_eq!(report.rows_after, 3);
    assert_eq!((report.inserted, report.updated, report.deleted), (1, 1, 1));
    assert_eq!(report.max_row_id, Some(3));
    assert_eq!(report.generation.id, 2);
    assert!(verify_dataset(&report.generation.path).unwrap().valid);

    let after = LogicalDataset::open(catalog.path()).unwrap();
    let a = query_email(&after, "a@example.com");
    assert_eq!(a.hits, 1);
    assert_eq!(a.rows[0].row_id, 0);
    let country = a.rows[0]
        .values
        .iter()
        .find(|x| x.column == "country")
        .unwrap();
    assert_eq!(country.value.as_deref(), Some("us"));

    assert_eq!(query_email(&after, "b@example.com").hits, 0);
    let c = query_email(&after, "c@example.com");
    assert_eq!(c.rows[0].row_id, 2);
    let d = query_email(&after, "d@example.com");
    assert_eq!(d.rows[0].row_id, 3);
    let note = d.rows[0]
        .values
        .iter()
        .find(|x| x.column == "note")
        .unwrap();
    assert_eq!(note.value.as_deref(), Some("NULL"));

    let generations = list_generations(catalog.path()).unwrap();
    assert_eq!(generations.len(), 2);
    assert!(generations[1].current);
}

#[test]
fn failed_mutation_leaves_current_generation_untouched() {
    let catalog = tempfile::tempdir().unwrap();
    let source = catalog.path().join("seed.csv");
    fs::write(&source, "email,country,note\na@example.com,pk,ok\n").unwrap();
    let initial = import_csv(catalog.path(), &source, &schema(), &import_config()).unwrap();
    let current = resolve_dataset_root(catalog.path()).unwrap();
    assert_eq!(current, initial.generation.path);

    let bad = vec![Mutation::Update {
        row_id: 999,
        values: BTreeMap::from([("country".into(), Some("us".into()))]),
    }];
    let error = apply_mutations(catalog.path(), &bad, &mutation_config()).unwrap_err();
    assert!(error.to_string().contains("does not exist"));
    assert_eq!(resolve_dataset_root(catalog.path()).unwrap(), current);
    assert_eq!(list_generations(catalog.path()).unwrap().len(), 1);
}

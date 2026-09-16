use lhr::{
    add_exact_hierarchies, build_u32_batches, dictionary_filename, write_schema, BuildConfig,
    ColumnSchema, DatasetSchema, Dictionary, HierarchySpec, LogicalDataset, LogicalType,
    Normalization,
};

#[test]
fn named_query_encodes_and_decodes_values() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("dictionaries")).unwrap();

    let country_path = d.path().join("dictionaries").join(dictionary_filename(0));
    let name_path = d.path().join("dictionaries").join(dictionary_filename(1));
    let country_card = Dictionary::build_from_values(["pk", "us"], &country_path, false).unwrap();
    let name_card = Dictionary::build_from_values(["alpha", "beta", "gamma"], &name_path, false).unwrap();
    let country = Dictionary::open(&country_path).unwrap();
    let names = Dictionary::open(&name_path).unwrap();

    let rows = [
        ("pk", "alpha"),
        ("us", "beta"),
        ("pk", "gamma"),
        ("pk", "beta"),
    ];
    let mut encoded = Vec::new();
    for (c, n) in rows {
        encoded.push(country.token(c).unwrap());
        encoded.push(names.token(n).unwrap());
    }

    let cfg = BuildConfig {
        columns: 2,
        page_rows: 2,
        cardinalities: vec![country_card, name_card],
        hierarchies: vec![],
        max_sort_records: 32,
    };
    build_u32_batches(vec![encoded], d.path(), &cfg).unwrap();
    add_exact_hierarchies(
        d.path(),
        &[
            HierarchySpec { columns: vec![0] },
            HierarchySpec { columns: vec![1] },
        ],
        32,
    )
    .unwrap();

    let schema = DatasetSchema::new(vec![
        ColumnSchema {
            name: "country".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::TrimLowercase,
            null_values: vec![],
        },
        ColumnSchema {
            name: "name".into(),
            logical_type: LogicalType::Text,
            nullable: false,
            normalization: Normalization::None,
            null_values: vec![],
        },
    ])
    .unwrap();
    write_schema(d.path(), &schema).unwrap();

    let logical = LogicalDataset::open(d.path()).unwrap();
    let result = logical
        .query_eq(
            &[("country".to_string(), "  PK  ".to_string())],
            Some(&["name".to_string()]),
            10,
        )
        .unwrap();
    assert_eq!(result.hits, 3);
    assert_eq!(result.returned, 3);
    let decoded: Vec<_> = result
        .rows
        .iter()
        .map(|row| row.values[0].value.as_deref().unwrap())
        .collect();
    assert_eq!(decoded, vec!["alpha", "gamma", "beta"]);

    let missing = logical
        .query_eq(
            &[("country".to_string(), "gb".to_string())],
            None,
            10,
        )
        .unwrap();
    assert_eq!(missing.hits, 0);
    assert_eq!(missing.rows_checked, 0);
}

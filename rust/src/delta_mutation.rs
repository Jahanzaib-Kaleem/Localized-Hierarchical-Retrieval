use crate::{
    abandon_generation, begin_generation, delta_path, import_csv, publish_generation, read_schema,
    write_overlay, write_schema, write_visibility, CsvImportConfig, DeltaLayerMeta, Manifest,
    Mutation, MutationConfig, MutationReport, RowIdWriter, VersionedDataset, VisibilityTarget,
    ROW_IDS_FILE,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io,
    path::Path,
};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn exact_accelerators(manifest: &Manifest) -> Vec<Vec<usize>> {
    let mut out = BTreeSet::new();
    for hierarchy in &manifest.hierarchies {
        if hierarchy.columns.len() < 2 {
            continue;
        }
        if matches!(
            hierarchy.kind.as_str(),
            "postings" | "densepost" | "deltapost" | "flatpost" | "bitslice"
        ) {
            out.insert(hierarchy.columns.clone());
        }
    }
    out.into_iter().collect()
}

fn clone_tree_link(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let source = entry.path();
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text == "temp"
            || text == "integrity.json"
            || text.ends_with(".tmp")
            || text.contains(".partial-")
        {
            continue;
        }
        let target = dst.join(name);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            clone_tree_link(&source, &target)?;
        } else if meta.is_file() {
            if fs::hard_link(&source, &target).is_err() {
                fs::copy(&source, &target)?;
            }
        }
    }
    Ok(())
}

fn canonical_value(
    dataset: &VersionedDataset,
    column: usize,
    value: &Option<String>,
) -> io::Result<Option<String>> {
    let schema = &dataset.schema().columns[column];
    match value {
        None => {
            if !schema.nullable {
                return Err(invalid(format!("column {} is not nullable", schema.name)));
            }
            Ok(None)
        }
        Some(raw) => Ok(Some(schema.canonicalize(raw)?)),
    }
}

struct Prepared {
    updates: BTreeMap<u64, BTreeMap<usize, Option<String>>>,
    deletes: BTreeSet<u64>,
    inserts: Vec<Vec<Option<String>>>,
    mutation_values: Vec<BTreeSet<String>>,
}

fn prepare(dataset: &VersionedDataset, mutations: &[Mutation]) -> io::Result<Prepared> {
    if mutations.is_empty() {
        return Err(invalid("mutation transaction is empty"));
    }
    let columns = dataset.schema().columns.len();
    let mut updates = BTreeMap::<u64, BTreeMap<usize, Option<String>>>::new();
    let mut deletes = BTreeSet::new();
    let mut inserts = Vec::new();
    let mut mutation_values = vec![BTreeSet::new(); columns];

    for mutation in mutations {
        match mutation {
            Mutation::Delete { row_id } => {
                if dataset.row_values(*row_id)?.is_none() {
                    return Err(invalid(format!("row_id {row_id} does not exist")));
                }
                deletes.insert(*row_id);
                updates.remove(row_id);
            }
            Mutation::Update { row_id, values } => {
                if dataset.row_values(*row_id)?.is_none() {
                    return Err(invalid(format!("row_id {row_id} does not exist")));
                }
                if deletes.contains(row_id) {
                    return Err(invalid(format!(
                        "row_id {row_id} was already deleted in this transaction"
                    )));
                }
                if values.is_empty() {
                    return Err(invalid(format!("update for row_id {row_id} has no values")));
                }
                let patch = updates.entry(*row_id).or_default();
                for (name, value) in values {
                    let column = dataset
                        .schema()
                        .column_index(name)
                        .ok_or_else(|| invalid(format!("unknown column {name}")))?;
                    let canonical = canonical_value(dataset, column, value)?;
                    if let Some(value) = canonical.as_ref() {
                        mutation_values[column].insert(value.clone());
                    }
                    patch.insert(column, canonical);
                }
            }
            Mutation::Insert { values } => {
                for name in values.keys() {
                    if dataset.schema().column_index(name).is_none() {
                        return Err(invalid(format!("unknown column {name}")));
                    }
                }
                let mut row = Vec::with_capacity(columns);
                for (column, schema) in dataset.schema().columns.iter().enumerate() {
                    let canonical = match values.get(&schema.name) {
                        Some(value) => canonical_value(dataset, column, value)?,
                        None if schema.nullable => None,
                        None => {
                            return Err(invalid(format!(
                                "insert is missing non-nullable column {}",
                                schema.name
                            )))
                        }
                    };
                    if let Some(value) = canonical.as_ref() {
                        mutation_values[column].insert(value.clone());
                    }
                    row.push(canonical);
                }
                inserts.push(row);
            }
        }
    }
    Ok(Prepared { updates, deletes, inserts, mutation_values })
}

fn null_sentinels(dataset: &VersionedDataset, prepared: &Prepared) -> Vec<Option<String>> {
    dataset
        .schema()
        .columns
        .iter()
        .enumerate()
        .map(|(column, schema)| {
            if !schema.nullable { return None; }
            for attempt in 0u64.. {
                let candidate = format!("\0LHR_DELTA_NULL_{column}_{attempt}\0");
                if !dataset.contains_canonical_value(column, &candidate)
                    && !prepared.mutation_values[column].contains(&candidate)
                {
                    return Some(candidate);
                }
            }
            unreachable!()
        })
        .collect()
}

fn temp_schema(
    schema: &crate::DatasetSchema,
    sentinels: &[Option<String>],
) -> crate::DatasetSchema {
    let mut schema = schema.clone();
    for (column, sentinel) in sentinels.iter().enumerate() {
        if let Some(sentinel) = sentinel {
            schema.columns[column].null_values = vec![sentinel.clone()];
        }
    }
    schema
}

fn remove_dir_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Apply row mutations as an append-only immutable delta layer.
///
/// Inserts and updated row versions are written only to the new layer. A compact sorted visibility
/// map redirects stable logical row IDs to their newest layer or to a tombstone. The base canonical
/// data and all previous delta layers remain hard-linked and untouched until explicit compaction.
pub fn apply_mutations_delta(
    catalog_root: impl AsRef<Path>,
    mutations: &[Mutation],
    config: &MutationConfig,
) -> io::Result<MutationReport> {
    if config.batch_rows == 0 || config.max_sort_records == 0 || config.dictionary_run_bytes == 0 {
        return Err(invalid(
            "batch_rows, max_sort_records, and dictionary_run_bytes must be > 0",
        ));
    }
    let catalog_root = catalog_root.as_ref();
    let stage = begin_generation(catalog_root)?;
    let work = catalog_root.join(format!(
        ".delta-work-{}-{}",
        stage.id,
        std::process::id()
    ));

    let result = (|| {
        let dataset = VersionedDataset::open(catalog_root)?;
        let source_root = dataset.root().to_path_buf();
        let schema = read_schema(&source_root)?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(source_root.join("manifest.json"))?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let prepared = prepare(&dataset, mutations)?;
        let rows_before = dataset.visible_rows();
        let rows_after = rows_before
            .checked_sub(prepared.deletes.len() as u64)
            .and_then(|x| x.checked_add(prepared.inserts.len() as u64))
            .ok_or_else(|| invalid("row count overflow"))?;
        if rows_after == 0 {
            return Err(invalid("deleting every row is not yet representable by LHR/1"));
        }

        fs::remove_dir_all(&stage.path)?;
        clone_tree_link(&source_root, &stage.path)?;
        fs::create_dir_all(&work)?;

        let mut overlay = dataset.overlay().clone();
        let mut visibility = dataset.visibility().to_map();
        let sentinels = null_sentinels(&dataset, &prepared);
        let internal_schema = temp_schema(&schema, &sentinels);
        let mut rows_for_delta = BTreeMap::<u64, Vec<Option<String>>>::new();

        for (&row_id, patch) in &prepared.updates {
            let mut values = dataset.row_values(row_id)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "updated row disappeared from immutable snapshot")
            })?;
            for (&column, value) in patch { values[column] = value.clone(); }
            rows_for_delta.insert(row_id, values);
        }

        let mut next_id = dataset.max_row_id().map_or(0, |x| x.saturating_add(1));
        for values in &prepared.inserts {
            if dataset.max_row_id() == Some(u64::MAX) || next_id == u64::MAX && !rows_for_delta.is_empty() {
                return Err(invalid("logical row ID overflow"));
            }
            rows_for_delta.insert(next_id, values.clone());
            next_id = next_id.checked_add(1).ok_or_else(|| invalid("logical row ID overflow"))?;
        }
        let max_row_id = if prepared.inserts.is_empty() {
            dataset.max_row_id()
        } else {
            next_id.checked_sub(1)
        };

        if !rows_for_delta.is_empty() {
            let layer_id = overlay.next_layer_id()?;
            let csv_path = work.join("delta.csv");
            let row_ids_path = work.join(ROW_IDS_FILE);
            let mut csv = csv::WriterBuilder::new()
                .from_path(&csv_path)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            csv.write_record(schema.columns.iter().map(|x| x.name.as_str()))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let mut row_ids = RowIdWriter::create(&row_ids_path, rows_for_delta.len() as u64)?;
            for (&row_id, values) in &rows_for_delta {
                let record: Vec<&str> = values
                    .iter()
                    .enumerate()
                    .map(|(column, value)| match value {
                        Some(value) => value.as_str(),
                        None => sentinels[column].as_deref().unwrap(),
                    })
                    .collect();
                csv.write_record(record)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                row_ids.push(row_id)?;
            }
            csv.flush()?;
            row_ids.finish()?;

            let build_catalog = work.join("build-catalog");
            let import_config = CsvImportConfig {
                page_rows: manifest.page_rows,
                batch_rows: config.batch_rows,
                max_sort_records: config.max_sort_records,
                dictionary_run_bytes: config.dictionary_run_bytes,
                accelerators: exact_accelerators(&manifest),
            };
            let built = import_csv(&build_catalog, &csv_path, &internal_schema, &import_config)?;
            let relative = delta_path(layer_id);
            let destination = stage.path.join(&relative);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::rename(&built.generation.path, &destination)?;
            let _ = fs::remove_file(destination.join("integrity.json"));
            let _ = fs::remove_file(destination.join(ROW_IDS_FILE));
            fs::rename(&row_ids_path, destination.join(ROW_IDS_FILE))?;
            write_schema(&destination, &schema)?;

            overlay.deltas.push(DeltaLayerMeta {
                id: layer_id,
                path: relative.to_string_lossy().replace('\\', "/"),
                rows: rows_for_delta.len() as u64,
            });
            for row_id in prepared.updates.keys() {
                visibility.insert(*row_id, VisibilityTarget::Layer(layer_id));
            }
        }

        for row_id in &prepared.deletes {
            visibility.insert(*row_id, VisibilityTarget::Deleted);
        }
        overlay.visible_rows = rows_after;
        overlay.max_row_id = max_row_id;
        write_overlay(&stage.path, &overlay)?;
        write_visibility(&stage.path, &visibility)?;

        Ok((
            rows_before,
            rows_after,
            prepared.inserts.len() as u64,
            prepared.updates.len() as u64,
            prepared.deletes.len() as u64,
            max_row_id,
        ))
    })();

    match result {
        Ok((rows_before, rows_after, inserted, updated, deleted, max_row_id)) => {
            let _ = remove_dir_if_exists(&work);
            let generation = publish_generation(stage)?;
            Ok(MutationReport {
                generation,
                rows_before,
                rows_after,
                inserted,
                updated,
                deleted,
                max_row_id,
            })
        }
        Err(error) => {
            let _ = remove_dir_if_exists(&work);
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

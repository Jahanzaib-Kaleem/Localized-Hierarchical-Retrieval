use crate::{
    abandon_generation, begin_generation, import_csv, publish_generation, read_schema,
    resolve_dataset_root, write_schema, CsvImportConfig, GenerationInfo, LogicalDataset, Manifest,
    RowIdWriter, ROW_IDS_FILE,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Mutation {
    Insert {
        values: BTreeMap<String, Option<String>>,
    },
    Update {
        row_id: u64,
        values: BTreeMap<String, Option<String>>,
    },
    Delete {
        row_id: u64,
    },
}

#[derive(Debug, Clone)]
pub struct MutationConfig {
    pub batch_rows: usize,
    pub max_sort_records: usize,
    pub dictionary_run_bytes: usize,
}

impl Default for MutationConfig {
    fn default() -> Self {
        Self {
            batch_rows: 16_384,
            max_sort_records: 250_000,
            dictionary_run_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MutationReport {
    pub generation: GenerationInfo,
    pub rows_before: u64,
    pub rows_after: u64,
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub max_row_id: Option<u64>,
}

struct Prepared {
    updates: BTreeMap<u64, BTreeMap<usize, Option<String>>>,
    deletes: BTreeSet<u64>,
    inserts: Vec<Vec<Option<String>>>,
    mutation_values: Vec<BTreeSet<String>>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn csv_error(error: csv::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn canonical_value(
    dataset: &LogicalDataset,
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
        // Mutation JSON represents NULL as JSON null. A string that happens to equal a CSV
        // null literal is therefore still an ordinary literal value here.
        Some(raw) => Ok(Some(schema.canonicalize(raw)?)),
    }
}

fn prepare(dataset: &LogicalDataset, mutations: &[Mutation]) -> io::Result<Prepared> {
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
                if dataset.physical_row_id(*row_id).is_none() {
                    return Err(invalid(format!("row_id {row_id} does not exist")));
                }
                deletes.insert(*row_id);
                updates.remove(row_id);
            }
            Mutation::Update { row_id, values } => {
                if dataset.physical_row_id(*row_id).is_none() {
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

    Ok(Prepared {
        updates,
        deletes,
        inserts,
        mutation_values,
    })
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

fn null_sentinels(dataset: &LogicalDataset, prepared: &Prepared) -> Vec<Option<String>> {
    dataset
        .schema()
        .columns
        .iter()
        .enumerate()
        .map(|(column, schema)| {
            if !schema.nullable {
                return None;
            }
            for attempt in 0u64.. {
                let candidate = format!("\0LHR_INTERNAL_NULL_{column}_{attempt}\0");
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

fn write_mutation_csv(
    dataset: &LogicalDataset,
    prepared: &Prepared,
    path: &Path,
    row_ids_path: &Path,
    sentinels: &[Option<String>],
) -> io::Result<(u64, Option<u64>)> {
    let rows_after = dataset
        .physical_rows()
        .checked_sub(prepared.deletes.len() as u64)
        .and_then(|x| x.checked_add(prepared.inserts.len() as u64))
        .ok_or_else(|| invalid("row count overflow"))?;
    if rows_after == 0 {
        return Err(invalid(
            "deleting every row is not yet representable by the current LHR/1 dictionary format",
        ));
    }

    let mut csv = csv::WriterBuilder::new()
        .from_path(path)
        .map_err(csv_error)?;
    csv.write_record(dataset.schema().columns.iter().map(|x| x.name.as_str()))
        .map_err(csv_error)?;
    let mut row_ids = RowIdWriter::create(row_ids_path, rows_after)?;

    for physical in 0..dataset.physical_rows() {
        let logical = dataset.logical_row_id(physical).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "physical row is missing logical row ID")
        })?;
        if prepared.deletes.contains(&logical) {
            continue;
        }
        let mut values = dataset.decode_physical_values(physical)?;
        if let Some(patch) = prepared.updates.get(&logical) {
            for (&column, value) in patch {
                values[column] = value.clone();
            }
        }
        let record: Vec<&str> = values
            .iter()
            .enumerate()
            .map(|(column, value)| match value {
                Some(value) => value.as_str(),
                None => sentinels[column].as_deref().unwrap(),
            })
            .collect();
        csv.write_record(record).map_err(csv_error)?;
        row_ids.push(logical)?;
    }

    let mut next_id = dataset.max_row_id().unwrap_or(0);
    if dataset.physical_rows() > 0 {
        next_id = next_id
            .checked_add(1)
            .ok_or_else(|| invalid("logical row ID overflow"))?;
    }
    for row in &prepared.inserts {
        let record: Vec<&str> = row
            .iter()
            .enumerate()
            .map(|(column, value)| match value {
                Some(value) => value.as_str(),
                None => sentinels[column].as_deref().unwrap(),
            })
            .collect();
        csv.write_record(record).map_err(csv_error)?;
        row_ids.push(next_id)?;
        next_id = next_id
            .checked_add(1)
            .ok_or_else(|| invalid("logical row ID overflow"))?;
    }
    csv.flush()?;
    row_ids.finish()?;
    let max_row_id = if prepared.inserts.is_empty() {
        (0..dataset.physical_rows())
            .rev()
            .filter_map(|physical| dataset.logical_row_id(physical))
            .find(|id| !prepared.deletes.contains(id))
    } else {
        next_id.checked_sub(1)
    };
    Ok((rows_after, max_row_id))
}

fn temp_schema(dataset: &LogicalDataset, sentinels: &[Option<String>]) -> crate::DatasetSchema {
    let mut schema = dataset.schema().clone();
    for (column, sentinel) in sentinels.iter().enumerate() {
        if let Some(sentinel) = sentinel {
            schema.columns[column].null_values = vec![sentinel.clone()];
        }
    }
    schema
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Apply a batch of row mutations as one crash-safe immutable generation transaction.
///
/// Row IDs are logical and stable: updates retain the existing ID, deletes leave a gap, and
/// inserts allocate monotonically increasing IDs. The current implementation deliberately
/// rebuilds the physical generation so all exact indexes and dictionaries are immediately
/// consistent; later delta/compaction layers can optimize write amplification without changing
/// these semantics.
pub fn apply_mutations(
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
        ".mutation-work-{}-{}",
        stage.id,
        std::process::id()
    ));

    let result = (|| {
        let current_root = resolve_dataset_root(catalog_root)?;
        let dataset = LogicalDataset::open(&current_root)?;
        let schema = read_schema(&current_root)?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(current_root.join("manifest.json"))?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let prepared = prepare(&dataset, mutations)?;
        let sentinels = null_sentinels(&dataset, &prepared);
        let internal_schema = temp_schema(&dataset, &sentinels);

        fs::create_dir_all(&work)?;
        let csv_path = work.join("mutation.csv");
        let row_ids_path = work.join(ROW_IDS_FILE);
        let (rows_after, max_row_id) = write_mutation_csv(
            &dataset,
            &prepared,
            &csv_path,
            &row_ids_path,
            &sentinels,
        )?;

        let build_catalog = work.join("build-catalog");
        let import_config = CsvImportConfig {
            page_rows: manifest.page_rows,
            batch_rows: config.batch_rows,
            max_sort_records: config.max_sort_records,
            dictionary_run_bytes: config.dictionary_run_bytes,
            accelerators: exact_accelerators(&manifest),
        };
        let built = import_csv(&build_catalog, &csv_path, &internal_schema, &import_config)?;

        // Move the already-validated physical build into our writer-locked staged generation,
        // then add stable row IDs and restore the public schema before the final seal/publish.
        fs::remove_dir_all(&stage.path)?;
        fs::rename(&built.generation.path, &stage.path)?;
        let _ = fs::remove_file(stage.path.join("integrity.json"));
        fs::rename(&row_ids_path, stage.path.join(ROW_IDS_FILE))?;
        write_schema(&stage.path, &schema)?;

        Ok((
            dataset.physical_rows(),
            rows_after,
            prepared.inserts.len() as u64,
            prepared.updates.len() as u64,
            prepared.deletes.len() as u64,
            max_row_id,
        ))
    })();

    match result {
        Ok((rows_before, rows_after, inserted, updated, deleted, max_row_id)) => {
            let _ = remove_if_exists(&work);
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
            let _ = remove_if_exists(&work);
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

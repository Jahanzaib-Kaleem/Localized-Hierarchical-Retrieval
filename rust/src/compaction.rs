use crate::{
    abandon_generation, begin_generation, import_csv, publish_generation, read_schema, write_schema,
    CsvImportConfig, GenerationInfo, Manifest, RowIdWriter, VersionedDataset, ROW_IDS_FILE,
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs,
    io,
    path::Path,
};

#[derive(Debug, Clone)]
pub struct CompactionConfig {
    pub batch_rows: usize,
    pub max_sort_records: usize,
    pub dictionary_run_bytes: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            batch_rows: 16_384,
            max_sort_records: 250_000,
            dictionary_run_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactionReport {
    pub generation: GenerationInfo,
    pub rows: u64,
    pub delta_layers_before: usize,
    pub visibility_overrides_before: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

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

fn null_sentinels(dataset: &VersionedDataset) -> Vec<Option<String>> {
    dataset
        .schema()
        .columns
        .iter()
        .enumerate()
        .map(|(column, schema)| {
            if !schema.nullable {
                return None;
            }
            if let Some(raw) = schema.null_values.first() {
                return Some(raw.clone());
            }
            for attempt in 0u64.. {
                let candidate = format!("\0LHR_COMPACTION_NULL_{column}_{attempt}\0");
                if !dataset.contains_canonical_value(column, &candidate) {
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
        if schema.columns[column].nullable {
            if let Some(sentinel) = sentinel {
                if !schema.columns[column].null_values.iter().any(|x| x == sentinel) {
                    schema.columns[column].null_values = vec![sentinel.clone()];
                }
            }
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

/// Merge all immutable delta layers and visibility overrides into one clean base generation.
/// Visible rows are streamed in stable logical-row-ID order, so compaction does not need a
/// database-sized in-memory row-ID set.
pub fn compact_dataset(
    catalog_root: impl AsRef<Path>,
    config: &CompactionConfig,
) -> io::Result<CompactionReport> {
    if config.batch_rows == 0 || config.max_sort_records == 0 || config.dictionary_run_bytes == 0 {
        return Err(invalid(
            "batch_rows, max_sort_records, and dictionary_run_bytes must be > 0",
        ));
    }
    let catalog_root = catalog_root.as_ref();
    let stage = begin_generation(catalog_root)?;
    let work = catalog_root.join(format!(
        ".compaction-work-{}-{}",
        stage.id,
        std::process::id()
    ));

    let result = (|| {
        let dataset = VersionedDataset::open(catalog_root)?;
        if dataset.visible_rows() == 0 {
            return Err(invalid("compaction cannot materialize an empty LHR/1 dataset"));
        }
        let source_root = dataset.root().to_path_buf();
        let schema = read_schema(&source_root)?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(source_root.join("manifest.json"))?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let bytes_before = crate::dataset_status(&source_root)?.total_bytes;
        let delta_layers_before = dataset.delta_meta().len();
        let visibility_overrides_before = dataset.visibility().len();
        let sentinels = null_sentinels(&dataset);
        let internal_schema = temp_schema(&schema, &sentinels);

        fs::create_dir_all(&work)?;
        let csv_path = work.join("compaction.csv");
        let row_ids_path = work.join(ROW_IDS_FILE);
        let mut csv = csv::WriterBuilder::new()
            .from_path(&csv_path)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        csv.write_record(schema.columns.iter().map(|x| x.name.as_str()))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut row_ids = RowIdWriter::create(&row_ids_path, dataset.visible_rows())?;
        let mut written = 0u64;
        dataset.for_each_visible_row(|row_id, values| {
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
            written += 1;
            Ok(())
        })?;
        if written != dataset.visible_rows() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "visible row count changed during immutable compaction snapshot",
            ));
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

        fs::remove_dir_all(&stage.path)?;
        fs::rename(&built.generation.path, &stage.path)?;
        let _ = fs::remove_file(stage.path.join("integrity.json"));
        fs::rename(&row_ids_path, stage.path.join(ROW_IDS_FILE))?;
        write_schema(&stage.path, &schema)?;

        Ok((
            dataset.visible_rows(),
            delta_layers_before,
            visibility_overrides_before,
            bytes_before,
        ))
    })();

    match result {
        Ok((rows, delta_layers_before, visibility_overrides_before, bytes_before)) => {
            let _ = remove_dir_if_exists(&work);
            let generation = publish_generation(stage)?;
            let bytes_after = crate::dataset_status(&generation.path)?.total_bytes;
            Ok(CompactionReport {
                generation,
                rows,
                delta_layers_before,
                visibility_overrides_before,
                bytes_before,
                bytes_after,
            })
        }
        Err(error) => {
            let _ = remove_dir_if_exists(&work);
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

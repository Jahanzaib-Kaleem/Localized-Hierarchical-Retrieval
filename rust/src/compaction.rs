use crate::{
    abandon_generation, add_exact_hierarchies, begin_generation, dictionary_filename,
    publish_generation, read_schema, read_storage_layout, write_schema, BuildConfig, Dictionary,
    GenerationInfo, HierarchySpec, Manifest, RowIdWriter, StorageMode, VersionedDataset, ROW_IDS_FILE,
};
use crate::dictionary_build::DictionarySpool;
use crate::stream_builder::U32StreamBuilder;
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
            dictionary_run_bytes: 16 * 1024 * 1024,
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

fn read_manifest(root: &Path) -> io::Result<Manifest> {
    serde_json::from_slice(&fs::read(root.join("manifest.json"))?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn exact_kind(kind: &str) -> bool {
    matches!(
        kind,
        "postings" | "densepost" | "deltapost" | "flatpost" | "bitslice"
    )
}

fn exact_specs(
    manifest: &Manifest,
    base_schema: &crate::DatasetSchema,
    logical_schema: &crate::DatasetSchema,
) -> io::Result<Vec<HierarchySpec>> {
    let mut set = BTreeSet::<Vec<usize>>::new();
    for column in 0..logical_schema.columns.len() {
        set.insert(vec![column]);
    }
    for hierarchy in &manifest.hierarchies {
        if hierarchy.columns.len() < 2 || !exact_kind(&hierarchy.kind) {
            continue;
        }
        let mut mapped = Vec::with_capacity(hierarchy.columns.len());
        for &base_column in &hierarchy.columns {
            let name = base_schema
                .columns
                .get(base_column)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "base accelerator references an out-of-range column",
                    )
                })?
                .name
                .as_str();
            mapped.push(logical_schema.column_index(name).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("compaction union schema lost indexed column {name:?}"),
                )
            })?);
        }
        mapped.sort_unstable();
        mapped.dedup();
        if mapped.len() == hierarchy.columns.len() {
            set.insert(mapped);
        }
    }
    Ok(set
        .into_iter()
        .map(|columns| HierarchySpec { columns })
        .collect())
}

fn preserved_hybrid_profile(
    source_root: &Path,
    manifest: &Manifest,
    base_schema: &crate::DatasetSchema,
    logical_schema: &crate::DatasetSchema,
) -> io::Result<(Option<Vec<usize>>, usize)> {
    let layout = read_storage_layout(source_root, manifest.columns, manifest.rows)?;
    if layout.mode != StorageMode::HybridParquet {
        return Ok((None, 0));
    }
    let mut cold = Vec::with_capacity(layout.cold_columns.len());
    for base_column in layout.cold_columns {
        let name = base_schema
            .columns
            .get(base_column)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "hybrid layout references an out-of-range base column",
                )
            })?
            .name
            .as_str();
        if let Some(logical) = logical_schema.column_index(name) {
            cold.push(logical);
        }
    }
    cold.sort_unstable();
    cold.dedup();
    if cold.is_empty() {
        return Ok((None, 0));
    }
    // Newly evolved columns default hot. This is conservative for latency and avoids silently
    // pushing a new field to cold storage merely because older generations never contained it.
    if cold.len() == logical_schema.columns.len() {
        cold.pop();
    }
    Ok((Some(cold), layout.row_group_rows.max(1)))
}

/// Merge all immutable delta layers and visibility overrides into one clean base generation.
///
/// Compaction is fully streaming: it performs one visible-row pass to rebuild dictionaries and a
/// second pass to emit dictionary tokens. It never creates a decoded CSV copy of the database.
/// Existing hybrid generations preserve their cold-column profile and Parquet row-group setting;
/// native generations remain native.
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
    let dataset = VersionedDataset::open(catalog_root)?;
    if dataset.visible_rows() == 0 {
        return Err(invalid("compaction cannot materialize an empty LHR/1 dataset"));
    }
    let source_root = dataset.root().to_path_buf();
    let base_schema = read_schema(&source_root)?;
    let logical_schema = dataset.schema().clone();
    let manifest = read_manifest(&source_root)?;
    let specs = exact_specs(&manifest, &base_schema, &logical_schema)?;
    let (cold_columns, row_group_rows) = preserved_hybrid_profile(
        &source_root,
        &manifest,
        &base_schema,
        &logical_schema,
    )?;
    let bytes_before = crate::dataset_status(&source_root)?.total_bytes;
    let delta_layers_before = dataset.delta_meta().len();
    let visibility_overrides_before = dataset.visibility().len();
    let rows = dataset.visible_rows();

    let stage = begin_generation(catalog_root)?;
    let result = (|| {
        let mut spool = DictionarySpool::create(&stage.path, logical_schema.columns.len())?;
        let mut first_rows = 0u64;
        dataset.for_each_visible_row(|_, values| {
            if values.len() != logical_schema.columns.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "versioned row width does not match union schema during compaction",
                ));
            }
            for (column, value) in values.iter().enumerate() {
                if let Some(value) = value {
                    spool.push(column, value)?;
                } else if !logical_schema.columns[column].nullable {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "visible row contains NULL in non-nullable column {}",
                            logical_schema.columns[column].name
                        ),
                    ));
                }
            }
            first_rows += 1;
            Ok(())
        })?;
        if first_rows != rows {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "visible row count changed during dictionary compaction pass",
            ));
        }
        let cardinalities = spool.finish(&logical_schema, config.dictionary_run_bytes)?;
        let dictionaries = (0..logical_schema.columns.len())
            .map(|column| {
                Dictionary::open(stage.path.join("dictionaries").join(dictionary_filename(column)))
            })
            .collect::<io::Result<Vec<_>>>()?;

        let build_cfg = BuildConfig {
            columns: logical_schema.columns.len(),
            page_rows: manifest.page_rows,
            cardinalities,
            hierarchies: Vec::new(),
            max_sort_records: config.max_sort_records,
        };
        let mut builder = U32StreamBuilder::create(
            &stage.path,
            build_cfg,
            cold_columns.clone(),
            row_group_rows,
        )?;
        let mut row_ids = RowIdWriter::create(stage.path.join(ROW_IDS_FILE), rows)?;
        let columns = logical_schema.columns.len();
        let mut batch = Vec::<u32>::with_capacity(config.batch_rows.saturating_mul(columns));
        let mut second_rows = 0u64;
        dataset.for_each_visible_row(|row_id, values| {
            for (column, value) in values.iter().enumerate() {
                let token = match value {
                    None => 0,
                    Some(value) => dictionaries[column].token(value).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "compaction value for column {} is absent from rebuilt dictionary",
                                logical_schema.columns[column].name
                            ),
                        )
                    })?,
                };
                batch.push(token);
            }
            row_ids.push(row_id)?;
            second_rows += 1;
            if batch.len() / columns >= config.batch_rows {
                builder.push_batch(std::mem::take(&mut batch))?;
                batch = Vec::with_capacity(config.batch_rows.saturating_mul(columns));
            }
            Ok(())
        })?;
        if !batch.is_empty() {
            builder.push_batch(batch)?;
        }
        row_ids.finish()?;
        if second_rows != rows {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "visible row count changed during token compaction pass",
            ));
        }
        let built = builder.finish()?;
        if built.rows != rows {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("compaction stream wrote {} rows, expected {rows}", built.rows),
            ));
        }
        add_exact_hierarchies(&stage.path, &specs, config.max_sort_records)?;
        write_schema(&stage.path, &logical_schema)?;
        let _ = fs::remove_dir_all(stage.path.join("temp"));
        Ok(())
    })();

    match result {
        Ok(()) => {
            drop(dataset);
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
            drop(dataset);
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

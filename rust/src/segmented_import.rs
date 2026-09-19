use crate::{
    abandon_generation, begin_generation, delta_path, import_csv, publish_generation, read_schema,
    verify_versioned_dataset, write_overlay, CsvImportConfig, CsvImportProgress, CsvImportStage,
    DeltaLayerMeta, GenerationInfo, Manifest, OverlayCatalog, RowIdWriter, VersionedDataset,
    ROW_IDS_FILE,
};
use csv::{ReaderBuilder, StringRecord, Writer, WriterBuilder};
use fs2::available_space;
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

pub const DEFAULT_SEGMENTED_PART_ROWS: u64 = 1_000_000;
pub const DEFAULT_SEGMENTED_PART_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_SEGMENTED_DICTIONARY_RUN_BYTES: usize = 16 * 1024 * 1024;
pub const SEGMENTED_PART_HEADROOM_MULTIPLIER: u64 = 4;
pub const SEGMENTED_IMPORT_FIXED_HEADROOM_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SegmentedCsvImportConfig {
    pub engine: CsvImportConfig,
    pub part_rows: u64,
    pub part_bytes: u64,
    /// Studio uploads are disposable staging copies. On Linux filesystems that support sparse
    /// hole punching, consumed source ranges can therefore be reclaimed after a part has been
    /// built into the unpublished generation.
    pub reclaim_consumed_source: bool,
}

impl SegmentedCsvImportConfig {
    pub fn from_engine(engine: CsvImportConfig) -> Self {
        let mut engine = engine;
        engine.dictionary_run_bytes = engine
            .dictionary_run_bytes
            .min(DEFAULT_SEGMENTED_DICTIONARY_RUN_BYTES);
        Self {
            engine,
            part_rows: DEFAULT_SEGMENTED_PART_ROWS,
            part_bytes: DEFAULT_SEGMENTED_PART_BYTES,
            reclaim_consumed_source: false,
        }
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.part_rows == 0 || self.part_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "segmented import part_rows and part_bytes must be > 0",
            ));
        }
        if self.engine.page_rows == 0
            || self.engine.batch_rows == 0
            || self.engine.max_sort_records == 0
            || self.engine.dictionary_run_bytes == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "segmented import engine resource limits must be > 0",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentedCsvImportReport {
    pub generation: GenerationInfo,
    pub rows: u64,
    pub parts: u32,
    pub exact_hierarchies: usize,
    pub source_bytes_reclaimed: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentedCsvAppendReport {
    pub generation: GenerationInfo,
    pub rows_before: u64,
    pub appended: u64,
    pub rows_after: u64,
    pub parts: u32,
    pub max_row_id: Option<u64>,
    pub source_bytes_reclaimed: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn csv_error(error: csv::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn remove_dir_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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

fn validate_headers(headers: &StringRecord, schema: &crate::DatasetSchema) -> io::Result<()> {
    let mut seen = BTreeSet::new();
    for header in headers.iter() {
        if !seen.insert(header.to_owned()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate CSV header {header:?}"),
            ));
        }
        if schema.column_index(header).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CSV has unexpected column {header:?}"),
            ));
        }
    }
    if headers.len() != schema.columns.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "CSV column count differs from schema: expected {}, found {}",
                schema.columns.len(),
                headers.len()
            ),
        ));
    }
    for column in &schema.columns {
        if !seen.contains(&column.name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CSV is missing required column {:?}", column.name),
            ));
        }
    }
    Ok(())
}

fn new_part_writer(path: &Path, headers: &StringRecord) -> io::Result<Writer<File>> {
    let mut writer = WriterBuilder::new().from_path(path).map_err(csv_error)?;
    writer.write_record(headers).map_err(csv_error)?;
    Ok(writer)
}

fn record_weight(record: &StringRecord) -> u64 {
    record
        .iter()
        .map(|value| value.len() as u64 + 1)
        .sum::<u64>()
        .max(1)
}

fn ensure_part_headroom(root: &Path, part_path: &Path) -> io::Result<()> {
    let part_bytes = fs::metadata(part_path)?.len();
    let required = part_bytes
        .saturating_mul(SEGMENTED_PART_HEADROOM_MULTIPLIER)
        .saturating_add(SEGMENTED_IMPORT_FIXED_HEADROOM_BYTES);
    let available = available_space(root)?;
    if available < required {
        return Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            format!(
                "insufficient free disk for next segmented import part: need at least {required} bytes, found {available}"
            ),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn reclaim_range(file: &File, start: u64, end: u64) -> u64 {
    use std::os::fd::AsRawFd;
    if end <= start || end > i64::MAX as u64 {
        return 0;
    }
    let length = end - start;
    if length > i64::MAX as u64 {
        return 0;
    }
    let result = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            start as libc::off_t,
            length as libc::off_t,
        )
    };
    if result == 0 { length } else { 0 }
}

#[cfg(not(target_os = "linux"))]
fn reclaim_range(_file: &File, _start: u64, _end: u64) -> u64 {
    0
}

fn mapped_exact_accelerators(
    manifest: &Manifest,
    base_schema: &crate::DatasetSchema,
    incoming_schema: &crate::DatasetSchema,
) -> Vec<Vec<usize>> {
    let mut out = BTreeSet::new();
    for hierarchy in &manifest.hierarchies {
        if hierarchy.columns.len() < 2
            || !matches!(
                hierarchy.kind.as_str(),
                "postings" | "densepost" | "deltapost" | "flatpost" | "bitslice"
            )
        {
            continue;
        }
        let mut mapped = Vec::with_capacity(hierarchy.columns.len());
        let mut complete = true;
        for &physical in &hierarchy.columns {
            let Some(base_column) = base_schema.columns.get(physical) else {
                complete = false;
                break;
            };
            let Some(incoming) = incoming_schema.column_index(&base_column.name) else {
                complete = false;
                break;
            };
            mapped.push(incoming);
        }
        if complete {
            mapped.sort_unstable();
            mapped.dedup();
            if mapped.len() == hierarchy.columns.len() {
                out.insert(mapped);
            }
        }
    }
    out.into_iter().collect()
}

fn ensure_append_schema_compatible(
    existing: &crate::DatasetSchema,
    incoming: &crate::DatasetSchema,
) -> io::Result<()> {
    incoming.validate()?;
    for actual in &incoming.columns {
        let Some(index) = existing.column_index(&actual.name) else {
            continue;
        };
        let expected = &existing.columns[index];
        if actual.logical_type != expected.logical_type
            || actual.normalization != expected.normalization
            || actual.null_values != expected.null_values
        {
            return Err(invalid(format!(
                "append schema conflict for column {:?}: shared column semantics changed",
                expected.name
            )));
        }
    }
    Ok(())
}

fn attach_layer(
    stage_root: &Path,
    overlay: &mut OverlayCatalog,
    built_root: &Path,
    rows: u64,
    first_row_id: u64,
) -> io::Result<()> {
    let layer_id = overlay.next_layer_id()?;
    let relative = delta_path(layer_id);
    let destination = stage_root.join(&relative);
    fs::create_dir_all(destination.parent().unwrap())?;
    fs::rename(built_root, &destination)?;
    let _ = fs::remove_file(destination.join("integrity.json"));
    let _ = fs::remove_file(destination.join(ROW_IDS_FILE));

    let mut row_ids = RowIdWriter::create(destination.join(ROW_IDS_FILE), rows)?;
    for offset in 0..rows {
        row_ids.push(
            first_row_id
                .checked_add(offset)
                .ok_or_else(|| invalid("logical row ID overflow"))?,
        )?;
    }
    row_ids.finish()?;

    overlay.deltas.push(DeltaLayerMeta {
        id: layer_id,
        path: relative.to_string_lossy().replace('\\', "/"),
        rows,
    });
    overlay.visible_rows = overlay
        .visible_rows
        .checked_add(rows)
        .ok_or_else(|| invalid("row count overflow"))?;
    overlay.max_row_id = first_row_id
        .checked_add(rows.checked_sub(1).ok_or_else(|| invalid("empty part"))?);
    write_overlay(stage_root, overlay)
}

fn for_each_csv_part<F, P>(
    catalog_root: &Path,
    csv_path: &Path,
    schema: &crate::DatasetSchema,
    config: &SegmentedCsvImportConfig,
    work: &Path,
    mut progress: P,
    mut consume: F,
) -> io::Result<(u64, u32, u64)>
where
    F: FnMut(&Path, u32, u64, u64) -> io::Result<()>,
    P: FnMut(CsvImportProgress),
{
    let source = File::open(csv_path)?;
    let mut reader = ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(source);
    let headers = reader.headers().map_err(csv_error)?.clone();
    validate_headers(&headers, schema)?;

    fs::create_dir_all(work)?;
    let mut part_index = 0u32;
    let mut total_rows = 0u64;
    let mut reclaimed = 0u64;
    let mut reclaimed_through = 0u64;
    let mut part_rows = 0u64;
    let mut part_weight = 0u64;
    let mut part_path = work.join(format!("part-{part_index:06}.csv"));
    let mut writer = Some(new_part_writer(&part_path, &headers)?);
    let mut record = StringRecord::new();

    progress(CsvImportProgress {
        stage: CsvImportStage::Parsing,
        rows_parsed: Some(0),
    });

    loop {
        let has_record = reader.read_record(&mut record).map_err(csv_error)?;
        if !has_record {
            break;
        }
        total_rows = total_rows
            .checked_add(1)
            .ok_or_else(|| invalid("row count overflow"))?;
        part_rows += 1;
        part_weight = part_weight.saturating_add(record_weight(&record));
        writer
            .as_mut()
            .unwrap()
            .write_record(&record)
            .map_err(csv_error)?;

        if total_rows % 65_536 == 0 {
            progress(CsvImportProgress {
                stage: CsvImportStage::Parsing,
                rows_parsed: Some(total_rows),
            });
        }

        if part_rows >= config.part_rows || part_weight >= config.part_bytes {
            let mut finished = writer.take().unwrap();
            finished.flush()?;
            drop(finished);
            ensure_part_headroom(catalog_root, &part_path)?;
            progress(CsvImportProgress {
                stage: CsvImportStage::Building,
                rows_parsed: Some(total_rows),
            });
            let consumed_through = reader.position().byte();
            consume(&part_path, part_index, part_rows, total_rows)?;
            fs::remove_file(&part_path)?;
            if config.reclaim_consumed_source {
                let bytes = reclaim_range(reader.get_ref(), reclaimed_through, consumed_through);
                if bytes > 0 {
                    reclaimed = reclaimed.saturating_add(bytes);
                    reclaimed_through = consumed_through;
                }
            }
            part_index = part_index
                .checked_add(1)
                .ok_or_else(|| invalid("part count overflow"))?;
            part_rows = 0;
            part_weight = 0;
            part_path = work.join(format!("part-{part_index:06}.csv"));
            writer = Some(new_part_writer(&part_path, &headers)?);
            progress(CsvImportProgress {
                stage: CsvImportStage::Parsing,
                rows_parsed: Some(total_rows),
            });
        }
    }

    if part_rows > 0 {
        let mut finished = writer.take().unwrap();
        finished.flush()?;
        drop(finished);
        ensure_part_headroom(catalog_root, &part_path)?;
        progress(CsvImportProgress {
            stage: CsvImportStage::Building,
            rows_parsed: Some(total_rows),
        });
        let consumed_through = reader.position().byte();
        consume(&part_path, part_index, part_rows, total_rows)?;
        fs::remove_file(&part_path)?;
        if config.reclaim_consumed_source {
            let bytes = reclaim_range(reader.get_ref(), reclaimed_through, consumed_through);
            reclaimed = reclaimed.saturating_add(bytes);
        }
        part_index = part_index
            .checked_add(1)
            .ok_or_else(|| invalid("part count overflow"))?;
    } else {
        drop(writer.take());
        let _ = fs::remove_file(&part_path);
    }

    if total_rows == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CSV contains no data rows",
        ));
    }

    Ok((total_rows, part_index, reclaimed))
}

fn part_engine_config(config: &SegmentedCsvImportConfig) -> CsvImportConfig {
    let mut engine = config.engine.clone();
    engine.dictionary_run_bytes = engine
        .dictionary_run_bytes
        .min(DEFAULT_SEGMENTED_DICTIONARY_RUN_BYTES);
    engine
}

pub fn import_csv_segmented_initial_with_progress<P>(
    catalog_root: impl AsRef<Path>,
    csv_path: impl AsRef<Path>,
    schema: &crate::DatasetSchema,
    config: &SegmentedCsvImportConfig,
    mut progress: P,
) -> io::Result<SegmentedCsvImportReport>
where
    P: FnMut(CsvImportProgress),
{
    config.validate()?;
    schema.validate()?;
    let catalog_root = catalog_root.as_ref();
    let csv_path = csv_path.as_ref();
    let stage = begin_generation(catalog_root)?;

    match crate::resolve_dataset_root(catalog_root) {
        Ok(_) => {
            let _ = abandon_generation(stage);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "initial CSV import requires an empty catalog",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = abandon_generation(stage);
            return Err(error);
        }
    }

    let work = catalog_root.join(format!(
        ".segmented-import-work-{}-{}",
        stage.id,
        std::process::id()
    ));
    let engine = part_engine_config(config);
    let mut overlay: Option<OverlayCatalog> = None;
    let mut exact_hierarchies = 0usize;

    let result = for_each_csv_part(
        catalog_root,
        csv_path,
        schema,
        config,
        &work,
        |event| progress(event),
        |part_path, part_index, part_rows, _total_rows| {
            let build_catalog = work.join(format!("build-{part_index:06}"));
            remove_dir_if_exists(&build_catalog)?;
            let built = import_csv(&build_catalog, part_path, schema, &engine)?;
            exact_hierarchies = exact_hierarchies.saturating_add(built.exact_hierarchies);

            if part_index == 0 {
                fs::remove_dir_all(&stage.path)?;
                fs::rename(&built.generation.path, &stage.path)?;
                let _ = fs::remove_file(stage.path.join("integrity.json"));
                overlay = Some(OverlayCatalog::empty(
                    part_rows,
                    part_rows.checked_sub(1),
                ));
                write_overlay(&stage.path, overlay.as_ref().unwrap())?;
            } else {
                let first_row_id = overlay
                    .as_ref()
                    .and_then(|x| x.max_row_id)
                    .and_then(|x| x.checked_add(1))
                    .ok_or_else(|| invalid("logical row ID overflow"))?;
                attach_layer(
                    &stage.path,
                    overlay.as_mut().unwrap(),
                    &built.generation.path,
                    part_rows,
                    first_row_id,
                )?;
            }
            remove_dir_if_exists(&build_catalog)?;
            Ok(())
        },
    );

    match result {
        Ok((rows, parts, reclaimed)) => {
            let verified = verify_versioned_dataset(&stage.path)?;
            if !verified.valid {
                let _ = remove_dir_if_exists(&work);
                let message = verified.errors.join("; ");
                let _ = abandon_generation(stage);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("segmented staged generation is invalid: {message}"),
                ));
            }
            let _ = remove_dir_if_exists(&work);
            progress(CsvImportProgress {
                stage: CsvImportStage::Publishing,
                rows_parsed: Some(rows),
            });
            let generation = publish_generation(stage)?;
            Ok(SegmentedCsvImportReport {
                generation,
                rows,
                parts,
                exact_hierarchies,
                source_bytes_reclaimed: reclaimed,
            })
        }
        Err(error) => {
            let _ = remove_dir_if_exists(&work);
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

pub fn append_csv_segmented_with_progress<P>(
    catalog_root: impl AsRef<Path>,
    csv_path: impl AsRef<Path>,
    incoming_schema: &crate::DatasetSchema,
    config: &SegmentedCsvImportConfig,
    mut progress: P,
) -> io::Result<SegmentedCsvAppendReport>
where
    P: FnMut(CsvImportProgress),
{
    config.validate()?;
    let catalog_root = catalog_root.as_ref();
    let csv_path = csv_path.as_ref();
    let stage = begin_generation(catalog_root)?;
    let work = catalog_root.join(format!(
        ".segmented-append-work-{}-{}",
        stage.id,
        std::process::id()
    ));

    let result = (|| {
        let dataset = VersionedDataset::open(catalog_root)?;
        let source_root = dataset.root().to_path_buf();
        let logical_schema = dataset.schema().clone();
        ensure_append_schema_compatible(&logical_schema, incoming_schema)?;
        let base_schema = read_schema(&source_root)?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(source_root.join("manifest.json"))?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        fs::remove_dir_all(&stage.path)?;
        clone_tree_link(&source_root, &stage.path)?;
        fs::create_dir_all(&work)?;

        let rows_before = dataset.visible_rows();
        let mut overlay = dataset.overlay().clone();
        let mut next_row_id = dataset
            .max_row_id()
            .map(|x| x.checked_add(1).ok_or_else(|| invalid("logical row ID overflow")))
            .transpose()?
            .unwrap_or(0);
        let mut engine = part_engine_config(config);
        engine.page_rows = manifest.page_rows;
        engine.accelerators =
            mapped_exact_accelerators(&manifest, &base_schema, incoming_schema);
        drop(dataset);

        let (appended, parts, reclaimed) = for_each_csv_part(
            catalog_root,
            csv_path,
            incoming_schema,
            config,
            &work,
            |event| progress(event),
            |part_path, part_index, part_rows, _total_rows| {
                let build_catalog = work.join(format!("build-{part_index:06}"));
                remove_dir_if_exists(&build_catalog)?;
                let built = import_csv(&build_catalog, part_path, incoming_schema, &engine)?;
                attach_layer(
                    &stage.path,
                    &mut overlay,
                    &built.generation.path,
                    part_rows,
                    next_row_id,
                )?;
                next_row_id = next_row_id
                    .checked_add(part_rows)
                    .ok_or_else(|| invalid("logical row ID overflow"))?;
                remove_dir_if_exists(&build_catalog)?;
                Ok(())
            },
        )?;

        let verified = verify_versioned_dataset(&stage.path)?;
        if !verified.valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "segmented append staged generation is invalid: {}",
                    verified.errors.join("; ")
                ),
            ));
        }

        Ok((rows_before, appended, parts, reclaimed, overlay.max_row_id))
    })();

    match result {
        Ok((rows_before, appended, parts, reclaimed, max_row_id)) => {
            let _ = remove_dir_if_exists(&work);
            progress(CsvImportProgress {
                stage: CsvImportStage::Publishing,
                rows_parsed: Some(appended),
            });
            let generation = publish_generation(stage)?;
            Ok(SegmentedCsvAppendReport {
                generation,
                rows_before,
                appended,
                rows_after: rows_before.saturating_add(appended),
                parts,
                max_row_id,
                source_bytes_reclaimed: reclaimed,
            })
        }
        Err(error) => {
            let _ = remove_dir_if_exists(&work);
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

pub fn segmented_import_disk_floor(bytes_total: u64) -> u64 {
    bytes_total
        .saturating_mul(2)
        .saturating_add(DEFAULT_SEGMENTED_PART_BYTES.saturating_mul(4))
        .saturating_add(SEGMENTED_IMPORT_FIXED_HEADROOM_BYTES)
}

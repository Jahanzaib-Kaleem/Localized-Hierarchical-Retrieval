use crate::{
    abandon_generation, add_exact_hierarchies, begin_generation, build_u32_batches,
    dictionary_filename, publish_generation, resolve_dataset_root, write_dictionary_record,
    write_schema, BuildConfig, DatasetSchema, Dictionary, GenerationInfo, HierarchySpec,
};
use csv::{Reader, ReaderBuilder, StringRecord};
use serde::Serialize;
use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{BTreeSet, BinaryHeap, HashMap},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    rc::Rc,
};

#[derive(Debug, Clone)]
pub struct CsvImportConfig {
    pub page_rows: usize,
    pub batch_rows: usize,
    pub max_sort_records: usize,
    pub dictionary_run_bytes: usize,
    /// Additional exact accelerators. Exact single-column indexes are always added for every
    /// column, so these normally contain pairs or wider combinations.
    pub accelerators: Vec<Vec<usize>>,
}

impl Default for CsvImportConfig {
    fn default() -> Self {
        Self {
            page_rows: 1024,
            batch_rows: 16_384,
            max_sort_records: 250_000,
            dictionary_run_bytes: 64 * 1024 * 1024,
            accelerators: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CsvImportReport {
    pub generation: GenerationInfo,
    pub rows: u64,
    pub cardinalities: Vec<u64>,
    pub exact_hierarchies: usize,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CsvImportStage {
    Validating,
    Parsing,
    Building,
    Indexing,
    Publishing,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct CsvImportProgress {
    pub stage: CsvImportStage,
    pub rows_parsed: Option<u64>,
}

fn csv_error(error: csv::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn contextual(error: io::Error, row: u64, column: &str) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("CSV row {row}, column {column}: {error}"),
    )
}

fn header_map(headers: &StringRecord, schema: &DatasetSchema) -> io::Result<Vec<usize>> {
    let mut positions = HashMap::<&str, usize>::new();
    for (index, header) in headers.iter().enumerate() {
        if positions.insert(header, index).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate CSV header {header:?}"),
            ));
        }
    }
    let unexpected = headers
        .iter()
        .filter(|header| schema.column_index(header).is_none())
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CSV has unexpected column(s): {}", unexpected.join(", ")),
        ));
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

    schema
        .columns
        .iter()
        .map(|column| {
            positions.get(column.name.as_str()).copied().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("CSV is missing required column {:?}", column.name),
                )
            })
        })
        .collect()
}

fn open_csv(path: &Path, schema: &DatasetSchema) -> io::Result<(Reader<File>, Vec<usize>)> {
    let mut reader = ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .from_path(path)
        .map_err(csv_error)?;
    let headers = reader.headers().map_err(csv_error)?.clone();
    let map = header_map(&headers, schema)?;
    Ok((reader, map))
}

fn read_record<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0usize;
    while got < len.len() {
        let n = reader.read(&mut len[got..])?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated dictionary sort record",
            ));
        }
        got += n;
    }
    let n = u32::from_le_bytes(len) as usize;
    let mut value = vec![0u8; n];
    reader.read_exact(&mut value)?;
    Ok(Some(value))
}

fn write_record_bytes<W: Write>(writer: &mut W, value: &[u8]) -> io::Result<()> {
    let len = u32::try_from(value.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "dictionary value exceeds u32 length")
    })?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(value)
}

fn flush_run(
    records: &mut Vec<Vec<u8>>,
    run_dir: &Path,
    run_number: usize,
) -> io::Result<PathBuf> {
    records.sort_unstable();
    records.dedup();
    let path = run_dir.join(format!("run-{run_number:06}.bin"));
    let mut writer = BufWriter::new(File::create(&path)?);
    for record in records.iter() {
        write_record_bytes(&mut writer, record)?;
    }
    writer.flush()?;
    records.clear();
    Ok(path)
}

#[derive(Eq)]
struct HeapItem {
    value: Vec<u8>,
    run: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value && self.run == other.run
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering so BinaryHeap behaves as a min-heap by value.
        other
            .value
            .cmp(&self.value)
            .then_with(|| other.run.cmp(&self.run))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// External sort for variable-length length-prefixed dictionary records. Run memory is bounded
/// by `max_run_bytes` plus individual value sizes. The merged output is sorted and deduplicated.
fn external_sort_dictionary(
    input: &Path,
    output: &Path,
    run_dir: &Path,
    max_run_bytes: usize,
) -> io::Result<()> {
    if max_run_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dictionary_run_bytes must be > 0",
        ));
    }
    fs::create_dir_all(run_dir)?;
    let mut reader = BufReader::new(File::open(input)?);
    let mut records = Vec::<Vec<u8>>::new();
    let mut bytes = 0usize;
    let mut runs = Vec::new();
    while let Some(record) = read_record(&mut reader)? {
        bytes = bytes.saturating_add(record.len() + 4);
        records.push(record);
        if bytes >= max_run_bytes {
            let run = flush_run(&mut records, run_dir, runs.len())?;
            runs.push(run);
            bytes = 0;
        }
    }
    if !records.is_empty() {
        let run = flush_run(&mut records, run_dir, runs.len())?;
        runs.push(run);
    }

    let mut output_writer = BufWriter::new(File::create(output)?);
    if runs.is_empty() {
        output_writer.flush()?;
        return Ok(());
    }

    let mut readers = runs
        .iter()
        .map(File::open)
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .map(BufReader::new)
        .collect::<Vec<_>>();
    let mut heap = BinaryHeap::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(value) = read_record(reader)? {
            heap.push(HeapItem { value, run });
        }
    }
    let mut previous: Option<Vec<u8>> = None;
    while let Some(item) = heap.pop() {
        if previous.as_deref() != Some(item.value.as_slice()) {
            write_record_bytes(&mut output_writer, &item.value)?;
            previous = Some(item.value.clone());
        }
        if let Some(value) = read_record(&mut readers[item.run])? {
            heap.push(HeapItem {
                value,
                run: item.run,
            });
        }
    }
    output_writer.flush()?;
    drop(output_writer);
    for run in runs {
        let _ = fs::remove_file(run);
    }
    let _ = fs::remove_dir(run_dir);
    Ok(())
}

fn first_pass(
    csv_path: &Path,
    schema: &DatasetSchema,
    stage: &Path,
    max_run_bytes: usize,
) -> io::Result<(u64, Vec<u64>)> {
    let temp = stage.join("temp").join("dictionaries");
    let dictionaries_dir = stage.join("dictionaries");
    fs::create_dir_all(&temp)?;
    fs::create_dir_all(&dictionaries_dir)?;

    let raw_paths: Vec<_> = (0..schema.columns.len())
        .map(|column| temp.join(format!("c{column:04}.raw")))
        .collect();
    let mut writers = raw_paths
        .iter()
        .map(File::create)
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .map(BufWriter::new)
        .collect::<Vec<_>>();

    let (mut reader, map) = open_csv(csv_path, schema)?;
    let mut record = StringRecord::new();
    let mut rows = 0u64;
    while reader.read_record(&mut record).map_err(csv_error)? {
        rows += 1;
        for (column_index, column) in schema.columns.iter().enumerate() {
            let raw = record.get(map[column_index]).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "CSV record missing mapped field")
            })?;
            if column.is_null_literal(raw) {
                continue;
            }
            let canonical = column
                .canonicalize(raw)
                .map_err(|e| contextual(e, rows + 1, &column.name))?;
            write_dictionary_record(&mut writers[column_index], &canonical)?;
        }
    }
    for writer in &mut writers {
        writer.flush()?;
    }
    drop(writers);

    if rows == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CSV contains no data rows",
        ));
    }

    let mut cardinalities = Vec::with_capacity(schema.columns.len());
    for (column_index, column) in schema.columns.iter().enumerate() {
        let sorted = temp.join(format!("c{column_index:04}.sorted"));
        let run_dir = temp.join(format!("runs-c{column_index:04}"));
        external_sort_dictionary(&raw_paths[column_index], &sorted, &run_dir, max_run_bytes)?;
        let output = dictionaries_dir.join(dictionary_filename(column_index));
        let cardinality = Dictionary::build_from_sorted_records(&sorted, &output, column.nullable)?;
        if cardinality == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("column {} has zero cardinality", column.name),
            ));
        }
        cardinalities.push(cardinality);
        let _ = fs::remove_file(&raw_paths[column_index]);
        let _ = fs::remove_file(sorted);
    }
    Ok((rows, cardinalities))
}

struct CsvTokenBatches {
    reader: Reader<File>,
    map: Vec<usize>,
    schema: DatasetSchema,
    dictionaries: Vec<Dictionary>,
    batch_rows: usize,
    row_number: u64,
    error: Rc<RefCell<Option<io::Error>>>,
}

impl CsvTokenBatches {
    fn new(
        csv_path: &Path,
        schema: DatasetSchema,
        stage: &Path,
        batch_rows: usize,
        error: Rc<RefCell<Option<io::Error>>>,
    ) -> io::Result<Self> {
        if batch_rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch_rows must be > 0",
            ));
        }
        let (reader, map) = open_csv(csv_path, &schema)?;
        let dictionaries = (0..schema.columns.len())
            .map(|column| {
                Dictionary::open(stage.join("dictionaries").join(dictionary_filename(column)))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            reader,
            map,
            schema,
            dictionaries,
            batch_rows,
            row_number: 0,
            error,
        })
    }

    fn fail(&mut self, error: io::Error) -> Option<Vec<u32>> {
        *self.error.borrow_mut() = Some(error);
        None
    }
}

impl Iterator for CsvTokenBatches {
    type Item = Vec<u32>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.error.borrow().is_some() {
            return None;
        }
        let columns = self.schema.columns.len();
        let mut out = Vec::with_capacity(self.batch_rows.saturating_mul(columns));
        let mut record = StringRecord::new();
        for _ in 0..self.batch_rows {
            let has_record = match self.reader.read_record(&mut record) {
                Ok(value) => value,
                Err(error) => return self.fail(csv_error(error)),
            };
            if !has_record {
                break;
            }
            self.row_number += 1;
            for column_index in 0..columns {
                let column = &self.schema.columns[column_index];
                let Some(raw) = record.get(self.map[column_index]) else {
                    return self.fail(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "CSV record missing mapped field",
                    ));
                };
                let token = if column.is_null_literal(raw) {
                    0u32
                } else {
                    let canonical = match column.canonicalize(raw) {
                        Ok(value) => value,
                        Err(error) => {
                            return self.fail(contextual(error, self.row_number + 1, &column.name))
                        }
                    };
                    match self.dictionaries[column_index].token(&canonical) {
                        Some(token) => token,
                        None => {
                            return self.fail(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "CSV row {}, column {} contains value absent from first-pass dictionary",
                                    self.row_number + 1,
                                    column.name
                                ),
                            ))
                        }
                    }
                };
                out.push(token);
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

fn exact_specs(columns: usize, accelerators: &[Vec<usize>]) -> io::Result<Vec<HierarchySpec>> {
    let mut set = BTreeSet::<Vec<usize>>::new();
    for column in 0..columns {
        set.insert(vec![column]);
    }
    for input in accelerators {
        if input.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "accelerators must contain at least two columns",
            ));
        }
        let mut columns_in_index = input.clone();
        columns_in_index.sort_unstable();
        columns_in_index.dedup();
        if columns_in_index.len() != input.len() || columns_in_index.iter().any(|&x| x >= columns) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "accelerator contains duplicate or invalid column",
            ));
        }
        set.insert(columns_in_index);
    }
    Ok(set
        .into_iter()
        .map(|columns| HierarchySpec { columns })
        .collect())
}

fn build_stage<F>(
    stage: &Path,
    csv_path: &Path,
    schema: &DatasetSchema,
    config: &CsvImportConfig,
    progress: &mut F,
) -> io::Result<(u64, Vec<u64>, usize)>
where
    F: FnMut(CsvImportProgress),
{
    if config.page_rows == 0 || config.max_sort_records == 0 || config.dictionary_run_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "page_rows, max_sort_records, and dictionary_run_bytes must be > 0",
        ));
    }
    progress(CsvImportProgress { stage: CsvImportStage::Validating, rows_parsed: None });
    schema.validate()?;
    progress(CsvImportProgress { stage: CsvImportStage::Parsing, rows_parsed: None });
    let (expected_rows, cardinalities) =
        first_pass(csv_path, schema, stage, config.dictionary_run_bytes)?;

    progress(CsvImportProgress { stage: CsvImportStage::Building, rows_parsed: Some(expected_rows) });
    let error = Rc::new(RefCell::new(None));
    let batches = CsvTokenBatches::new(
        csv_path,
        schema.clone(),
        stage,
        config.batch_rows,
        Rc::clone(&error),
    )?;
    let build_cfg = BuildConfig {
        columns: schema.columns.len(),
        page_rows: config.page_rows,
        cardinalities: cardinalities.clone(),
        hierarchies: Vec::new(),
        max_sort_records: config.max_sort_records,
    };
    let manifest = build_u32_batches(batches, stage, &build_cfg)?;
    if let Some(error) = error.borrow_mut().take() {
        return Err(error);
    }
    if manifest.rows != expected_rows {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "CSV changed between dictionary and encode passes: expected {expected_rows} rows, encoded {}",
                manifest.rows
            ),
        ));
    }

    progress(CsvImportProgress { stage: CsvImportStage::Indexing, rows_parsed: Some(expected_rows) });
    let specs = exact_specs(schema.columns.len(), &config.accelerators)?;
    add_exact_hierarchies(stage, &specs, config.max_sort_records)?;
    write_schema(stage, schema)?;
    Ok((expected_rows, cardinalities, specs.len()))
}

/// Two-pass CSV import into a new immutable catalog generation. The old CURRENT generation is not
/// modified unless dictionary construction, encoding, exact-index construction, verification, and
/// integrity sealing all succeed.
fn import_csv_with_progress_mode<F>(
    catalog_root: impl AsRef<Path>,
    csv_path: impl AsRef<Path>,
    schema: &DatasetSchema,
    config: &CsvImportConfig,
    require_empty: bool,
    mut progress: F,
) -> io::Result<CsvImportReport>
where
    F: FnMut(CsvImportProgress),
{
    let catalog_root = catalog_root.as_ref();
    let csv_path = csv_path.as_ref();
    let stage = begin_generation(catalog_root)?;
    if require_empty {
        match resolve_dataset_root(catalog_root) {
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
    }
    let built = build_stage(&stage.path, csv_path, schema, config, &mut progress);
    let (rows, cardinalities, exact_hierarchies) = match built {
        Ok(value) => value,
        Err(error) => {
            let _ = abandon_generation(stage);
            return Err(error);
        }
    };
    progress(CsvImportProgress { stage: CsvImportStage::Publishing, rows_parsed: Some(rows) });
    let generation = publish_generation(stage)?;
    Ok(CsvImportReport {
        generation,
        rows,
        cardinalities,
        exact_hierarchies,
    })
}

pub fn import_csv_with_progress<F>(
    catalog_root: impl AsRef<Path>,
    csv_path: impl AsRef<Path>,
    schema: &DatasetSchema,
    config: &CsvImportConfig,
    progress: F,
) -> io::Result<CsvImportReport>
where
    F: FnMut(CsvImportProgress),
{
    import_csv_with_progress_mode(catalog_root, csv_path, schema, config, false, progress)
}

pub fn import_csv_initial_with_progress<F>(
    catalog_root: impl AsRef<Path>,
    csv_path: impl AsRef<Path>,
    schema: &DatasetSchema,
    config: &CsvImportConfig,
    progress: F,
) -> io::Result<CsvImportReport>
where
    F: FnMut(CsvImportProgress),
{
    import_csv_with_progress_mode(catalog_root, csv_path, schema, config, true, progress)
}

pub fn import_csv(
    catalog_root: impl AsRef<Path>,
    csv_path: impl AsRef<Path>,
    schema: &DatasetSchema,
    config: &CsvImportConfig,
) -> io::Result<CsvImportReport> {
    import_csv_with_progress(catalog_root, csv_path, schema, config, |_| {})
}

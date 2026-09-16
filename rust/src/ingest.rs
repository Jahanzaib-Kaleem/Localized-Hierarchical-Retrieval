use crate::{
    abandon_generation, begin_generation, import_csv, publish_generation, write_schema, CsvImportConfig,
    CsvImportReport, DatasetSchema, GenerationInfo,
};
use serde::{de::{Error as DeError, SeqAccess, Visitor}, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalFormat { Csv, Jsonl, Json }

#[derive(Debug, Clone)]
pub struct ExternalImportConfig {
    pub engine: CsvImportConfig,
    pub max_rejects: usize,
    pub reject_output: Option<PathBuf>,
    pub progress_path: Option<PathBuf>,
    pub progress_every: u64,
    pub resume_id: Option<String>,
    pub minimum_free_bytes: u64,
    pub reject_unknown_json_fields: bool,
}

impl Default for ExternalImportConfig {
    fn default() -> Self {
        Self {
            engine: CsvImportConfig::default(),
            max_rejects: 0,
            reject_output: None,
            progress_path: None,
            progress_every: 10_000,
            resume_id: None,
            minimum_free_bytes: 256 * 1024 * 1024,
            reject_unknown_json_fields: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExternalImportReport {
    pub generation: GenerationInfo,
    pub source_rows: u64,
    pub accepted_rows: u64,
    pub rejected_rows: u64,
    pub cardinalities: Vec<u64>,
    pub exact_hierarchies: usize,
    pub resumed_prepared_spool: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResumeState {
    source: String,
    source_bytes: u64,
    modified_secs: u64,
    format: ExternalFormat,
    prepared: bool,
    source_rows: u64,
    accepted_rows: u64,
    rejected_rows: u64,
}

#[derive(Debug, Serialize)]
struct RejectRecord<'a> {
    row: u64,
    error: &'a str,
    raw: String,
}

#[derive(Debug, Serialize)]
struct Progress {
    phase: &'static str,
    source_rows: u64,
    accepted_rows: u64,
    rejected_rows: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut file, value).map_err(json_error)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(tmp); }
    result
}

fn source_fingerprint(path: &Path) -> io::Result<(u64, u64)> {
    let meta = fs::metadata(path)?;
    let modified = meta.modified().ok()
        .and_then(|x| x.duration_since(UNIX_EPOCH).ok())
        .map(|x| x.as_secs()).unwrap_or(0);
    Ok((meta.len(), modified))
}

fn safe_resume_id(id: &str) -> io::Result<&str> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
        return Err(invalid("resume_id may contain only ASCII letters, digits, '-' and '_'"));
    }
    Ok(id)
}

fn work_dir(root: &Path, resume: Option<&str>) -> io::Result<(PathBuf, bool)> {
    if let Some(id) = resume {
        let id = safe_resume_id(id)?;
        Ok((root.join("ingest-resume").join(id), true))
    } else {
        Ok((root.join(format!(".ingest-work-{}", std::process::id())), false))
    }
}

fn check_disk(root: &Path, source_bytes: u64, minimum_free_bytes: u64) -> io::Result<()> {
    let free = fs2::available_space(root)?;
    let estimated = source_bytes.saturating_mul(3).saturating_add(minimum_free_bytes);
    if free < estimated {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            format!("ingest preflight requires about {estimated} free bytes, only {free} available"),
        ));
    }
    Ok(())
}

fn json_scalar(value: &Value) -> io::Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(x) => Ok(Some(x.clone())),
        Value::Number(x) => Ok(Some(x.to_string())),
        Value::Bool(x) => Ok(Some(if *x { "true" } else { "false" }.into())),
        _ => Err(invalid("JSON column value must be scalar or null")),
    }
}

fn null_sentinels(schema: &DatasetSchema) -> Vec<Option<String>> {
    schema.columns.iter().enumerate().map(|(column, spec)| {
        spec.nullable.then(|| format!("\0LHR_INGEST_NULL_{column}\0"))
    }).collect()
}

fn internal_schema(schema: &DatasetSchema, sentinels: &[Option<String>]) -> DatasetSchema {
    let mut out = schema.clone();
    for (index, sentinel) in sentinels.iter().enumerate() {
        if let Some(sentinel) = sentinel { out.columns[index].null_values = vec![sentinel.clone()]; }
    }
    out
}

struct Preparer<'a> {
    schema: &'a DatasetSchema,
    sentinels: &'a [Option<String>],
    csv: csv::Writer<File>,
    rejects: File,
    max_rejects: usize,
    reject_unknown_json_fields: bool,
    source_rows: u64,
    accepted_rows: u64,
    rejected_rows: u64,
    progress_path: Option<&'a Path>,
    progress_every: u64,
}

impl<'a> Preparer<'a> {
    fn progress(&self, phase: &'static str) -> io::Result<()> {
        if let Some(path) = self.progress_path {
            atomic_json(path, &Progress { phase, source_rows: self.source_rows, accepted_rows: self.accepted_rows, rejected_rows: self.rejected_rows })?;
        }
        Ok(())
    }

    fn reject(&mut self, row: u64, error: &str, raw: String) -> io::Result<()> {
        self.rejected_rows += 1;
        serde_json::to_writer(&mut self.rejects, &RejectRecord { row, error, raw }).map_err(json_error)?;
        self.rejects.write_all(b"\n")?;
        if self.rejected_rows as usize > self.max_rejects {
            return Err(invalid(format!("reject limit exceeded at source row {row}: {error}")));
        }
        Ok(())
    }

    fn validate_and_write(&mut self, row: u64, values: Vec<Option<String>>, raw: String) -> io::Result<()> {
        let result = (|| {
            if values.len() != self.schema.columns.len() { return Err(invalid("row has wrong column count")); }
            let mut record = Vec::with_capacity(values.len());
            for (column, value) in values.into_iter().enumerate() {
                let spec = &self.schema.columns[column];
                match value {
                    None => {
                        if !spec.nullable { return Err(invalid(format!("column {} is not nullable", spec.name))); }
                        record.push(self.sentinels[column].as_ref().unwrap().clone());
                    }
                    Some(value) => {
                        spec.canonicalize(&value)?;
                        record.push(value);
                    }
                }
            }
            self.csv.write_record(record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(())
        })();
        match result {
            Ok(()) => self.accepted_rows += 1,
            Err(error) => self.reject(row, &error.to_string(), raw)?,
        }
        if self.progress_every > 0 && self.source_rows % self.progress_every == 0 { self.progress("prepare")?; }
        Ok(())
    }

    fn json_value(&mut self, value: Value) -> io::Result<()> {
        self.source_rows += 1;
        let row = self.source_rows;
        let raw = serde_json::to_string(&value).unwrap_or_default();
        let object = match value.as_object() {
            Some(x) => x,
            None => return self.reject(row, "JSON row must be an object", raw),
        };
        if self.reject_unknown_json_fields {
            let known: BTreeSet<_> = self.schema.columns.iter().map(|x| x.name.as_str()).collect();
            if let Some(unknown) = object.keys().find(|key| !known.contains(key.as_str())) {
                return self.reject(row, &format!("unknown JSON field {unknown}"), raw);
            }
        }
        let mut values = Vec::with_capacity(self.schema.columns.len());
        for spec in &self.schema.columns {
            match object.get(&spec.name) {
                Some(value) => match json_scalar(value) {
                    Ok(value) => values.push(value),
                    Err(error) => return self.reject(row, &error.to_string(), raw),
                },
                None if spec.nullable => values.push(None),
                None => return self.reject(row, &format!("missing field {}", spec.name), raw),
            }
        }
        self.validate_and_write(row, values, raw)
    }
}

struct JsonArrayVisitor<'a, 'b> { preparer: &'a mut Preparer<'b> }
impl<'de, 'a, 'b> Visitor<'de> for JsonArrayVisitor<'a, 'b> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result { formatter.write_str("a JSON array of row objects") }
    fn visit_seq<A>(self, mut seq: A) -> Result<(), A::Error> where A: SeqAccess<'de> {
        while let Some(value) = seq.next_element::<Value>()? {
            self.preparer.json_value(value).map_err(A::Error::custom)?;
        }
        Ok(())
    }
}

fn prepare_json_array(path: &Path, preparer: &mut Preparer<'_>) -> io::Result<()> {
    let file = File::open(path)?;
    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
    Deserializer::deserialize_seq(&mut deserializer, JsonArrayVisitor { preparer }).map_err(json_error)
}

fn prepare_jsonl(path: &Path, preparer: &mut Preparer<'_>) -> io::Result<()> {
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        match serde_json::from_str::<Value>(&line) {
            Ok(value) => preparer.json_value(value)?,
            Err(error) => {
                preparer.source_rows += 1;
                preparer.reject(preparer.source_rows, &format!("invalid JSON: {error}"), line)?;
            }
        }
    }
    Ok(())
}

fn prepare_csv(path: &Path, preparer: &mut Preparer<'_>) -> io::Result<()> {
    let mut reader = csv::ReaderBuilder::new().has_headers(true).flexible(true).from_path(path)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let headers = reader.headers().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?.clone();
    let positions: BTreeMap<_, _> = headers.iter().enumerate().map(|(i, name)| (name.to_owned(), i)).collect();
    for spec in &preparer.schema.columns {
        if !positions.contains_key(&spec.name) { return Err(invalid(format!("CSV is missing required column {}", spec.name))); }
    }
    for result in reader.records() {
        preparer.source_rows += 1;
        let row = preparer.source_rows;
        let record = match result {
            Ok(x) => x,
            Err(error) => { preparer.reject(row, &format!("invalid CSV: {error}"), String::new())?; continue; }
        };
        let mut values = Vec::with_capacity(preparer.schema.columns.len());
        let mut raw = Vec::new();
        for spec in &preparer.schema.columns {
            let value = record.get(*positions.get(&spec.name).unwrap()).unwrap_or("").to_owned();
            raw.push(value.clone());
            if spec.is_null_literal(&value) { values.push(None); } else { values.push(Some(value)); }
        }
        preparer.validate_and_write(row, values, raw.join(","))?;
    }
    Ok(())
}

fn prepared_state_matches(state: &ResumeState, source: &Path, bytes: u64, modified: u64, format: ExternalFormat) -> bool {
    state.prepared && state.source == source.to_string_lossy() && state.source_bytes == bytes && state.modified_secs == modified && state.format == format
}

pub fn import_external(
    catalog_root: impl AsRef<Path>,
    source: impl AsRef<Path>,
    format: ExternalFormat,
    schema: &DatasetSchema,
    config: &ExternalImportConfig,
) -> io::Result<ExternalImportReport> {
    schema.validate()?;
    let root = catalog_root.as_ref();
    let source = source.as_ref();
    fs::create_dir_all(root)?;
    let (source_bytes, modified) = source_fingerprint(source)?;
    check_disk(root, source_bytes, config.minimum_free_bytes)?;
    let (work, persistent) = work_dir(root, config.resume_id.as_deref())?;
    fs::create_dir_all(&work)?;
    let state_path = work.join("state.json");
    let accepted_path = work.join("accepted.csv");
    let rejects_path = work.join("rejects.jsonl");
    let mut resumed = false;
    let state = if state_path.exists() {
        let state: ResumeState = serde_json::from_slice(&fs::read(&state_path)?).map_err(json_error)?;
        if prepared_state_matches(&state, source, source_bytes, modified, format) && accepted_path.is_file() {
            resumed = true;
            state
        } else {
            fs::remove_dir_all(&work)?;
            fs::create_dir_all(&work)?;
            ResumeState { source: source.to_string_lossy().into_owned(), source_bytes, modified_secs: modified, format, prepared: false, source_rows: 0, accepted_rows: 0, rejected_rows: 0 }
        }
    } else {
        ResumeState { source: source.to_string_lossy().into_owned(), source_bytes, modified_secs: modified, format, prepared: false, source_rows: 0, accepted_rows: 0, rejected_rows: 0 }
    };

    let state = if resumed { state } else {
        let sentinels = null_sentinels(schema);
        let csv_file = File::create(&accepted_path)?;
        let rejects = File::create(&rejects_path)?;
        let mut preparer = Preparer {
            schema, sentinels: &sentinels,
            csv: csv::WriterBuilder::new().from_writer(csv_file), rejects,
            max_rejects: config.max_rejects,
            reject_unknown_json_fields: config.reject_unknown_json_fields,
            source_rows: 0, accepted_rows: 0, rejected_rows: 0,
            progress_path: config.progress_path.as_deref(), progress_every: config.progress_every,
        };
        preparer.csv.write_record(schema.columns.iter().map(|x| x.name.as_str()))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        match format {
            ExternalFormat::Csv => prepare_csv(source, &mut preparer)?,
            ExternalFormat::Jsonl => prepare_jsonl(source, &mut preparer)?,
            ExternalFormat::Json => prepare_json_array(source, &mut preparer)?,
        }
        preparer.csv.flush()?;
        preparer.rejects.sync_all()?;
        preparer.progress("prepared")?;
        if preparer.accepted_rows == 0 { return Err(invalid("no accepted rows to import")); }
        let state = ResumeState {
            source: source.to_string_lossy().into_owned(), source_bytes, modified_secs: modified,
            format, prepared: true, source_rows: preparer.source_rows,
            accepted_rows: preparer.accepted_rows, rejected_rows: preparer.rejected_rows,
        };
        atomic_json(&state_path, &state)?;
        state
    };

    // JSON null requires an internal sentinel even when the public schema declares no textual
    // null literal. Build in a private catalog, then restore the public schema before publication.
    let sentinels = null_sentinels(schema);
    let internal = internal_schema(schema, &sentinels);
    let build_catalog = work.join("build-catalog");
    if build_catalog.exists() { fs::remove_dir_all(&build_catalog)?; }
    let CsvImportReport { generation: built, cardinalities, exact_hierarchies, .. } =
        import_csv(&build_catalog, &accepted_path, &internal, &config.engine)?;

    let stage = begin_generation(root)?;
    let publish_result = (|| {
        fs::remove_dir_all(&stage.path)?;
        fs::rename(&built.path, &stage.path)?;
        let _ = fs::remove_file(stage.path.join("integrity.json"));
        write_schema(&stage.path, schema)?;
        publish_generation(stage)
    })();
    let generation = match publish_result {
        Ok(generation) => generation,
        Err(error) => return Err(error),
    };

    if let Some(output) = &config.reject_output {
        if let Some(parent) = output.parent() { fs::create_dir_all(parent)?; }
        fs::copy(&rejects_path, output)?;
    }
    if let Some(path) = &config.progress_path {
        atomic_json(path, &Progress { phase: "complete", source_rows: state.source_rows, accepted_rows: state.accepted_rows, rejected_rows: state.rejected_rows })?;
    }
    if !persistent { let _ = fs::remove_dir_all(&work); }

    Ok(ExternalImportReport {
        generation,
        source_rows: state.source_rows,
        accepted_rows: state.accepted_rows,
        rejected_rows: state.rejected_rows,
        cardinalities,
        exact_hierarchies,
        resumed_prepared_spool: resumed,
    })
}

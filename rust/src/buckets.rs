use crate::{
    abandon_generation, apply_mutations_delta, begin_generation, dataset_stats, import_csv,
    leased_generation_ids, publish_generation, write_schema, CsvImportConfig, CsvImportReport,
    DatasetSchema, GenerationInfo, Mutation, MutationConfig, MutationReport, VersionedDataset,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_BUCKET: &str = "default";
const BUCKETS_DIR: &str = "buckets";
const BUCKET_META: &str = ".bucket.json";
const BUCKET_FORMAT: &str = "LHR-BUCKET/1";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BucketMeta {
    format: String,
    id: String,
    name: String,
    created_at_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub ready: bool,
    pub rows: u64,
    pub columns: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketCombineReport {
    pub target: BucketInfo,
    pub sources: Vec<String>,
    pub rows: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketTransferReport {
    pub source: String,
    pub destination: String,
    pub mode: String,
    pub rows_requested: usize,
    pub destination_report: MutationReport,
    pub source_report: Option<MutationReport>,
    pub warning: Option<String>,
}

pub fn validate_bucket_id(id: &str) -> io::Result<()> {
    if id == DEFAULT_BUCKET {
        return Ok(());
    }
    if id.is_empty() || id.len() > 64 {
        return Err(invalid("bucket id must contain 1..=64 characters"));
    }
    let bytes = id.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() {
        return Err(invalid("bucket id must start with a letter or digit"));
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_')
    {
        return Err(invalid(
            "bucket id may contain only lowercase letters, digits, hyphen and underscore",
        ));
    }
    Ok(())
}

pub fn bucket_root(root: impl AsRef<Path>, id: &str) -> io::Result<PathBuf> {
    validate_bucket_id(id)?;
    let root = root.as_ref();
    if id == DEFAULT_BUCKET {
        Ok(root.to_path_buf())
    } else {
        Ok(root.join(BUCKETS_DIR).join(id))
    }
}

pub fn require_bucket_root(root: impl AsRef<Path>, id: &str) -> io::Result<PathBuf> {
    let path = bucket_root(root, id)?;
    if id != DEFAULT_BUCKET && !path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("bucket {id:?} does not exist"),
        ));
    }
    Ok(path)
}

fn read_meta(path: &Path, id: &str) -> BucketMeta {
    fs::read(path.join(BUCKET_META))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BucketMeta>(&bytes).ok())
        .filter(|meta| meta.format == BUCKET_FORMAT && meta.id == id)
        .unwrap_or_else(|| BucketMeta {
            format: BUCKET_FORMAT.into(),
            id: id.into(),
            name: id.into(),
            created_at_ms: 0,
        })
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("bucket metadata path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".bucket-meta-{}.tmp", std::process::id()));
    let data = serde_json::to_vec_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(&temp, data)?;
    fs::rename(temp, path)?;
    Ok(())
}

fn info(root: &Path, id: &str, name: String) -> BucketInfo {
    match dataset_stats(root) {
        Ok(stats) => BucketInfo {
            id: id.into(),
            name,
            is_default: id == DEFAULT_BUCKET,
            ready: true,
            rows: stats.rows,
            columns: stats.columns,
            total_bytes: stats.total_bytes,
        },
        Err(_) => BucketInfo {
            id: id.into(),
            name,
            is_default: id == DEFAULT_BUCKET,
            ready: false,
            rows: 0,
            columns: 0,
            total_bytes: 0,
        },
    }
}

pub fn list_buckets(root: impl AsRef<Path>) -> io::Result<Vec<BucketInfo>> {
    let root = root.as_ref();
    let default_meta = read_meta(root, DEFAULT_BUCKET);
    let default_name = if default_meta.created_at_ms == 0 && default_meta.name == DEFAULT_BUCKET {
        "Default".into()
    } else {
        default_meta.name
    };
    let mut buckets = vec![info(root, DEFAULT_BUCKET, default_name)];
    let named_root = root.join(BUCKETS_DIR);
    if named_root.is_dir() {
        for entry in fs::read_dir(named_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if validate_bucket_id(&id).is_err() || id == DEFAULT_BUCKET {
                continue;
            }
            let path = entry.path();
            let meta = read_meta(&path, &id);
            buckets.push(info(&path, &id, meta.name));
        }
    }
    buckets[1..].sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    Ok(buckets)
}

pub fn create_bucket(root: impl AsRef<Path>, id: &str, name: &str) -> io::Result<BucketInfo> {
    validate_bucket_id(id)?;
    if id == DEFAULT_BUCKET {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "default is a reserved compatibility bucket",
        ));
    }
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 120 {
        return Err(invalid("bucket name must contain 1..=120 characters"));
    }
    let path = bucket_root(root.as_ref(), id)?;
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("bucket {id:?} already exists"),
        ));
    }
    fs::create_dir_all(&path)?;
    let meta = BucketMeta {
        format: BUCKET_FORMAT.into(),
        id: id.into(),
        name: trimmed.into(),
        created_at_ms: now_ms(),
    };
    if let Err(error) = atomic_write_json(&path.join(BUCKET_META), &meta) {
        let _ = fs::remove_dir_all(&path);
        return Err(error);
    }
    Ok(info(&path, id, meta.name))
}

pub fn rename_bucket(root: impl AsRef<Path>, id: &str, name: &str) -> io::Result<BucketInfo> {
    let path = require_bucket_root(root, id)?;
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 120 {
        return Err(invalid("bucket name must contain 1..=120 characters"));
    }
    let mut meta = read_meta(&path, id);
    meta.name = trimmed.into();
    if meta.created_at_ms == 0 {
        meta.created_at_ms = now_ms();
    }
    atomic_write_json(&path.join(BUCKET_META), &meta)?;
    Ok(info(&path, id, meta.name))
}

pub fn delete_bucket(root: impl AsRef<Path>, id: &str) -> io::Result<()> {
    if id == DEFAULT_BUCKET {
        return Err(invalid("the reserved default bucket cannot be deleted"));
    }
    let path = require_bucket_root(root, id)?;
    if !leased_generation_ids(&path)?.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "bucket has active snapshot readers",
        ));
    }
    fs::remove_dir_all(path)
}

fn choose_null_sentinels(
    schema: &DatasetSchema,
    datasets: &[VersionedDataset],
) -> Vec<Option<String>> {
    schema
        .columns
        .iter()
        .enumerate()
        .map(|(column, column_schema)| {
            if !column_schema.nullable {
                return None;
            }
            for attempt in 0u64.. {
                let candidate = format!(
                    "__LHR_BUCKET_NULL_{}_{}_{}_{}__",
                    std::process::id(),
                    now_ms(),
                    column,
                    attempt
                );
                if datasets
                    .iter()
                    .all(|dataset| !dataset.contains_canonical_value(column, &candidate))
                {
                    return Some(candidate);
                }
            }
            unreachable!()
        })
        .collect()
}


fn publish_csv_generation(
    catalog_root: &Path,
    csv_path: &Path,
    public_schema: &DatasetSchema,
    import_schema: &DatasetSchema,
    config: &CsvImportConfig,
) -> io::Result<(GenerationInfo, CsvImportReport)> {
    let stage = begin_generation(catalog_root)?;
    let build_catalog = catalog_root.join(format!(
        ".bucket-build-{}-{}",
        stage.id,
        std::process::id()
    ));
    let result = (|| -> io::Result<(GenerationInfo, CsvImportReport)> {
        let built = import_csv(&build_catalog, csv_path, import_schema, config)?;
        fs::remove_dir_all(&stage.path)?;
        fs::rename(&built.generation.path, &stage.path)?;
        let _ = fs::remove_file(stage.path.join("integrity.json"));
        write_schema(&stage.path, public_schema)?;
        let generation = publish_generation(stage)?;
        Ok((generation, built))
    })();
    let _ = fs::remove_dir_all(&build_catalog);
    result
}

pub fn combine_buckets(
    root: impl AsRef<Path>,
    sources: &[String],
    target_id: &str,
    target_name: &str,
    config: &CsvImportConfig,
) -> io::Result<BucketCombineReport> {
    if sources.is_empty() {
        return Err(invalid("at least one source bucket is required"));
    }
    let root = root.as_ref();
    let mut unique = BTreeSet::new();
    for source in sources {
        validate_bucket_id(source)?;
        if !unique.insert(source.clone()) {
            return Err(invalid(format!("duplicate source bucket {source:?}")));
        }
    }
    if unique.contains(target_id) {
        return Err(invalid("target bucket cannot also be a source"));
    }

    let mut datasets = Vec::with_capacity(sources.len());
    for source in sources {
        let source_root = require_bucket_root(root, source)?;
        datasets.push(VersionedDataset::open(source_root)?);
    }
    let schema = datasets
        .first()
        .ok_or_else(|| invalid("no source buckets"))?
        .schema()
        .clone();
    if datasets.iter().any(|dataset| dataset.schema() != &schema) {
        return Err(invalid("all combined buckets must have exactly the same schema"));
    }

    let created = create_bucket(root, target_id, target_name)?;
    let target_root = bucket_root(root, target_id)?;
    let temp_dir = root.join("temp");
    fs::create_dir_all(&temp_dir)?;
    let temp_csv = temp_dir.join(format!(
        "bucket-combine-{}-{}.csv",
        std::process::id(),
        now_ms()
    ));
    let sentinels = choose_null_sentinels(&schema, &datasets);
    let mut import_schema = schema.clone();
    for (column, sentinel) in sentinels.iter().enumerate() {
        if let Some(sentinel) = sentinel {
            import_schema.columns[column].null_values = vec![sentinel.clone()];
        }
    }

    let result = (|| -> io::Result<BucketCombineReport> {
        let mut writer = csv::WriterBuilder::new()
            .from_path(&temp_csv)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        writer
            .write_record(schema.columns.iter().map(|column| column.name.as_str()))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut rows = 0u64;
        for dataset in &datasets {
            dataset.for_each_visible_row(|_, values| {
                let record: Vec<&str> = values
                    .iter()
                    .enumerate()
                    .map(|(column, value)| match value {
                        Some(value) => value.as_str(),
                        None => sentinels[column].as_deref().unwrap(),
                    })
                    .collect();
                writer
                    .write_record(record)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                rows = rows.saturating_add(1);
                Ok(())
            })?;
        }
        writer
            .flush()
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
        if rows == 0 {
            return Err(invalid("cannot combine empty buckets into an LHR/1 dataset"));
        }
        let (_generation, _report) =
            publish_csv_generation(&target_root, &temp_csv, &schema, &import_schema, config)?;
        let target = info(&target_root, target_id, created.name.clone());
        Ok(BucketCombineReport {
            target,
            sources: sources.to_vec(),
            rows,
        })
    })();

    let _ = fs::remove_file(&temp_csv);
    if result.is_err() {
        let _ = fs::remove_dir_all(&target_root);
    }
    result
}

pub fn transfer_rows(
    root: impl AsRef<Path>,
    source: &str,
    destination: &str,
    row_ids: &[u64],
    move_rows: bool,
    config: &MutationConfig,
) -> io::Result<BucketTransferReport> {
    if source == destination {
        return Err(invalid("source and destination buckets must differ"));
    }
    if row_ids.is_empty() {
        return Err(invalid("row_ids is empty"));
    }
    let root = root.as_ref();
    let source_root = require_bucket_root(root, source)?;
    let destination_root = require_bucket_root(root, destination)?;
    let source_dataset = VersionedDataset::open(&source_root)?;
    let destination_dataset = VersionedDataset::open(&destination_root)?;
    if source_dataset.schema() != destination_dataset.schema() {
        return Err(invalid("source and destination bucket schemas differ"));
    }

    let unique: BTreeSet<u64> = row_ids.iter().copied().collect();
    if unique.len() != row_ids.len() {
        return Err(invalid("row_ids contains duplicates"));
    }
    if move_rows && unique.len() as u64 >= source_dataset.visible_rows() {
        return Err(invalid(
            "moving every row would create an empty LHR/1 bucket; combine/copy into a new bucket instead",
        ));
    }

    let mut inserts = Vec::with_capacity(unique.len());
    for row_id in &unique {
        let values = source_dataset
            .row_values(*row_id)?
            .ok_or_else(|| invalid(format!("row_id {row_id} does not exist in source bucket")))?;
        let values = source_dataset
            .schema()
            .columns
            .iter()
            .zip(values.into_iter())
            .map(|(column, value)| (column.name.clone(), value))
            .collect::<BTreeMap<_, _>>();
        inserts.push(Mutation::Insert { values });
    }
    drop(destination_dataset);
    let destination_report = apply_mutations_delta(&destination_root, &inserts, config)?;

    let mut source_report = None;
    let mut warning = None;
    if move_rows {
        let deletes = unique
            .iter()
            .map(|row_id| Mutation::Delete { row_id: *row_id })
            .collect::<Vec<_>>();
        drop(source_dataset);
        match apply_mutations_delta(&source_root, &deletes, config) {
            Ok(report) => source_report = Some(report),
            Err(error) => {
                warning = Some(format!(
                    "rows were copied to the destination but source deletion failed: {error}"
                ));
            }
        }
    }

    Ok(BucketTransferReport {
        source: source.into(),
        destination: destination.into(),
        mode: if move_rows { "move" } else { "copy" }.into(),
        rows_requested: unique.len(),
        destination_report,
        source_report,
        warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_ids_are_path_safe() {
        for valid in ["a", "leads", "matcha-2026", "client_01"] {
            assert!(validate_bucket_id(valid).is_ok());
        }
        for invalid in ["", "../x", "Upper", "a/b", ".hidden", "white space"] {
            assert!(validate_bucket_id(invalid).is_err());
        }
        assert!(validate_bucket_id(DEFAULT_BUCKET).is_ok());
    }

    #[test]
    fn default_bucket_reuses_legacy_catalog_root() {
        let root = Path::new("/data");
        assert_eq!(bucket_root(root, DEFAULT_BUCKET).unwrap(), root);
        assert_eq!(bucket_root(root, "leads").unwrap(), root.join("buckets/leads"));
    }
}

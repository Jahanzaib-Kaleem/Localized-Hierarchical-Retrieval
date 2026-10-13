//! Read-only LHR bucket backed directly by operator-verified Parquet shards.
//! This preserves all source columns without a 155+ GiB uncompressed import spool.

use super::McpState;
use lhr::{create_bucket, read_schema_file};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

// The VPS has less than 1 GiB physical RAM; never run multiple DuckDB scans concurrently.
static ATTACHED_QUERY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const FORMAT: &str = "LHR-ATTACHED-PARQUET/1";
const BUCKET_ID: &str = "apollo-full-50m";
const EXPECTED_ROWS: u64 = 50_761_482;
const EXPECTED_COLUMNS: usize = 114;
const EXPECTED_FILES: usize = 27;
const FILE_MARKER: &str = ".parquet_attachment.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileInfo {
    pub name: String,
    pub rows: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub format: String,
    pub row_count: u64,
    pub columns: usize,
    pub source_bytes: u64,
    pub files: Vec<FileInfo>,
}

fn source_dir(root: &Path) -> PathBuf {
    root.join("control").join("sandbox").join("apollo-import-source")
}

fn approved_manifest(root: &Path) -> Result<Manifest, String> {
    let dir = source_dir(root);
    let bytes = fs::read(dir.join("apollo-manifest.json")).map_err(|e| e.to_string())?;
    if bytes.len() > 64 * 1024 {
        return Err("Apollo manifest exceeds metadata size limit".into());
    }
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if manifest.format != FORMAT
        || manifest.row_count != EXPECTED_ROWS
        || manifest.columns != EXPECTED_COLUMNS
        || manifest.files.len() != EXPECTED_FILES
    {
        return Err("manifest does not describe the approved complete Apollo dataset".into());
    }
    let schema = read_schema_file(dir.join("apollo-schema.json")).map_err(|e| e.to_string())?;
    if schema.columns.len() != EXPECTED_COLUMNS {
        return Err("attached Parquet schema has the wrong number of columns".into());
    }
    let mut rows = 0_u64;
    let mut total_bytes = 0_u64;
    let mut previous = String::new();
    for entry in &manifest.files {
        let middle = entry.name.strip_prefix("apollo_data_")
            .and_then(|name| name.strip_suffix(".parquet"))
            .ok_or("invalid Parquet shard filename")?;
        if middle.len() != 4 || !middle.bytes().all(|b| b.is_ascii_digit()) {
            return Err("invalid Parquet shard filename".into());
        }
        if entry.name <= previous {
            return Err("Parquet shards must be unique and sorted".into());
        }
        previous = entry.name.clone();
        let path = dir.join(&entry.name);
        let meta = path.symlink_metadata().map_err(|e| e.to_string())?;
        if !meta.file_type().is_file() || meta.len() != entry.bytes || entry.rows == 0 {
            return Err(format!("Parquet source shard missing or changed: {}", entry.name));
        }
        rows = rows.checked_add(entry.rows).ok_or("Parquet row count overflow")?;
        total_bytes = total_bytes.checked_add(entry.bytes).ok_or("Parquet byte count overflow")?;
    }
    if rows != manifest.row_count || total_bytes != manifest.source_bytes {
        return Err("manifest source totals do not match its shards".into());
    }
    if fs::read(dir.join(".source-sha256")).map_err(|e| e.to_string())?
        != fs::read(dir.join(".copied-sha256")).map_err(|e| e.to_string())?
    {
        return Err("copied source verification records differ".into());
    }
    Ok(manifest)
}

pub(super) fn attached(bucket_root: &Path) -> Result<Option<Manifest>, String> {
    let path = bucket_root.join(FILE_MARKER);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if manifest.format != FORMAT {
        return Err("unsupported attached Parquet format".into());
    }
    Ok(Some(manifest))
}

pub(super) fn attach_staged_at_startup(state: &McpState) {
    let dir = source_dir(&state.root);
    let marker = dir.join(".attach-now.json");
    if !marker.is_file() {
        return;
    }
    let used = dir.join(".last-attach-request.json");
    if fs::rename(&marker, &used).is_err() {
        return;
    }
    let result = (|| -> Result<Value, String> {
        let bytes = fs::read(&used).map_err(|e| e.to_string())?;
        if bytes.len() > 4096 {
            return Err("attachment request is too large".into());
        }
        let args: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if args.get("action").and_then(Value::as_str) != Some("attach")
            || args.get("bucket_id").and_then(Value::as_str) != Some(BUCKET_ID)
        {
            return Err("attachment is restricted to the explicitly approved Apollo bucket".into());
        }
        let manifest = approved_manifest(&state.root)?;
        create_bucket(&state.root, BUCKET_ID, "Apollo Full Database (50.76M)")
            .map_err(|e| e.to_string())?;
        let root = state.root.join("buckets").join(BUCKET_ID);
        let temp = root.join(".parquet_attachment.tmp");
        let target = root.join(FILE_MARKER);
        let mut f = File::create(&temp).map_err(|e| e.to_string())?;
        f.write_all(&serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        fs::rename(temp, target).map_err(|e| e.to_string())?;
        Ok(json!({
            "bucket_id": BUCKET_ID,
            "ready": true,
            "mode": "attached_parquet",
            "rows": manifest.row_count,
            "columns": manifest.columns,
            "source_files": manifest.files.len(),
            "total_bytes": manifest.source_bytes
        }))
    })();
    let value = match result {
        Ok(data) => json!({"success": true, "result": data}),
        Err(error) => json!({"success": false, "error": error}),
    };
    let target = dir.join(".attach-result.json");
    let temp = dir.join(".attach-result.tmp");
    if let Ok(bytes) = serde_json::to_vec_pretty(&value) {
        if fs::write(&temp, bytes).is_ok() {
            let _ = fs::rename(temp, target);
        }
    }
}

pub(super) fn schema(state: &McpState) -> Result<Value, String> {
    let schema = read_schema_file(source_dir(&state.root).join("apollo-schema.json"))
        .map_err(|e| e.to_string())?;
    serde_json::to_value(schema).map_err(|e| e.to_string())
}

pub(super) fn stats(manifest: &Manifest) -> Value {
    json!({
        "format": FORMAT,
        "storage_mode": "attached_parquet",
        "rows": manifest.row_count,
        "columns": manifest.columns,
        "segments": manifest.files.len(),
        "canonical_bytes": manifest.source_bytes,
        "routing_bytes": 0,
        "total_bytes": manifest.source_bytes,
        "hierarchies": 0,
        "integrity_present": true,
        "read_only": true,
        "native_indexes": false
    })
}

pub(super) fn verify(state: &McpState) -> Result<Value, String> {
    let manifest = approved_manifest(&state.root)?;
    Ok(json!({
        "valid": true,
        "mode": "attached_parquet",
        "checked_files": manifest.files.len(),
        "rows_from_verified_manifest": manifest.row_count,
        "warnings": ["Attachment verification checks the existing checksum records and current shard sizes; it does not recompute the full 25 GiB SHA-256 hashes"]
    }))
}

pub(super) fn query(state: &McpState, mut request: Value) -> Result<Value, String> {
    request["operation"] = json!("query");
    run_worker(state, request)
}

pub(super) fn row(state: &McpState, bucket_id: &str, row_id: u64) -> Result<Value, String> {
    run_worker(state, json!({"operation":"row","bucket":bucket_id,"row_id":row_id,"timeout_ms":30000}))
}

fn run_worker(_state: &McpState, request: Value) -> Result<Value, String> {
    let _guard = ATTACHED_QUERY_LOCK.lock()
        .map_err(|_| "attached Parquet query lock poisoned".to_string())?;
    let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    if bytes.len() > 256 * 1024 {
        return Err("attached Parquet query exceeds size limit".into());
    }
    let mut process = Command::new("/usr/bin/python3")
        .arg("/usr/local/lib/lhr/parquet_bucket_query.py")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| e.to_string())?;
    if let Some(mut input) = process.stdin.take() {
        input.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    let output = process.wait_with_output().map_err(|e| e.to_string())?;
    if output.stdout.len() > 32 * 1024 * 1024 {
        return Err("attached Parquet query returned too much data".into());
    }
    let decoded: Value = serde_json::from_slice(&output.stdout).map_err(|e| {
        format!("attached Parquet worker returned invalid JSON: {e}; stderr: {}",
            String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(2048)]))
    })?;
    if !output.status.success() {
        return Err(decoded.get("error").and_then(Value::as_str)
            .unwrap_or("attached Parquet query failed").to_string());
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_preserve_attached_source_bytes_and_rows() {
        let manifest = Manifest {
            format: FORMAT.into(), row_count: EXPECTED_ROWS, columns: EXPECTED_COLUMNS,
            source_bytes: 27_364_131_966, files: vec![],
        };
        let value = stats(&manifest);
        assert_eq!(value["rows"], EXPECTED_ROWS);
        assert_eq!(value["total_bytes"], 27_364_131_966_u64);
        assert_eq!(value["storage_mode"], "attached_parquet");
    }
}

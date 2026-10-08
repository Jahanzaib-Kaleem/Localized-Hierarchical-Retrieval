use super::McpState;
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use lhr::{
    execute_query, require_bucket_root, LogicalType, QueryFilter, QueryRequest, VersionedDataset,
    DEFAULT_BUCKET,
};
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    fmt::Write as _,
    io,
    path::Path as FsPath,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const DEFAULT_EXPIRY_MINUTES: u64 = 60;
const MAX_EXPIRY_MINUTES: u64 = 24 * 60;
const MAX_ACTIVE_EXPORTS: usize = 1024;
const EXPORT_PAGE_ROWS: usize = 4096;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExportFormat {
    Csv,
    Jsonl,
}

impl Default for ExportFormat {
    fn default() -> Self {
        Self::Csv
    }
}

impl ExportFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Jsonl => "jsonl",
        }
    }

    fn mime_type(self) -> &'static str {
        match self {
            Self::Csv => "text/csv",
            Self::Jsonl => "application/x-ndjson",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Jsonl => "jsonl",
        }
    }
}

#[derive(Debug, Deserialize)]
struct ExportArgs {
    #[serde(default = "default_bucket")]
    bucket: String,
    #[serde(default)]
    filters: Vec<QueryFilter>,
    #[serde(default)]
    select: Vec<String>,
    #[serde(default)]
    after_row_id: Option<u64>,
    #[serde(default)]
    max_rows: Option<u64>,
    #[serde(default)]
    include_row_id: bool,
    #[serde(default)]
    format: ExportFormat,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default = "default_expiry_minutes")]
    expires_minutes: u64,
}

#[derive(Clone)]
pub(super) struct ExportGrant {
    bucket: String,
    filters: Vec<QueryFilter>,
    select: Vec<String>,
    after_row_id: Option<u64>,
    max_rows: Option<u64>,
    include_row_id: bool,
    format: ExportFormat,
    file_name: String,
    expires_at_epoch: u64,
}

pub(super) type ExportRegistry = Arc<Mutex<HashMap<String, ExportGrant>>>;

pub(super) struct ExportLink {
    pub structured: Value,
    pub uri: String,
    pub name: String,
    pub mime_type: String,
    pub description: String,
}

pub(super) fn new_registry() -> ExportRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

fn default_bucket() -> String {
    DEFAULT_BUCKET.into()
}

fn default_expiry_minutes() -> u64 {
    DEFAULT_EXPIRY_MINUTES
}

pub(super) fn tool_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "bucket":{"type":"string","default":"default"},
            "filters":{"type":"array","default":[],"items":{
                "type":"object","required":["op","column"],
                "properties":{
                    "op":{"type":"string","enum":["eq","in","range"]},
                    "column":{"type":"string"},
                    "value":{"type":["string","null"]},
                    "values":{"type":"array","items":{"type":["string","null"]}},
                    "gte":{"type":["string","null"]},
                    "lte":{"type":["string","null"]}
                },
                "additionalProperties":false
            }},
            "select":{"type":"array","items":{"type":"string"}},
            "after_row_id":{"type":["integer","null"],"minimum":0},
            "max_rows":{"type":["integer","null"],"minimum":1},
            "include_row_id":{"type":"boolean","default":false},
            "format":{"type":"string","enum":["csv","jsonl"],"default":"csv"},
            "file_name":{"type":["string","null"]},
            "expires_minutes":{"type":"integer","minimum":1,"maximum":1440,"default":60}
        },
        "additionalProperties":false
    })
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn purge_expired(exports: &mut HashMap<String, ExportGrant>, now: u64) {
    exports.retain(|_, grant| grant.expires_at_epoch > now);
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    let mut rng = OsRng;
    rng.fill_bytes(&mut bytes);
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut token, "{byte:02x}");
    }
    token
}

fn safe_file_name(requested: Option<&str>, bucket: &str, format: ExportFormat) -> String {
    let default_name = format!("{bucket}-export-{}.{}", now_epoch(), format.extension());
    let raw = requested
        .and_then(|value| FsPath::new(value).file_name())
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&default_name);
    let mut safe: String = raw
        .chars()
        .take(120)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() || safe == "." || safe == ".." {
        safe = "lhr-export".into();
    }
    let suffix = format!(".{}", format.extension());
    if !safe.to_ascii_lowercase().ends_with(&suffix) {
        safe.push_str(&suffix);
    }
    safe
}

fn validate_args(state: &McpState, args: &ExportArgs) -> Result<(), String> {
    if args.expires_minutes == 0 || args.expires_minutes > MAX_EXPIRY_MINUTES {
        return Err(format!(
            "expires_minutes must be 1..={MAX_EXPIRY_MINUTES}"
        ));
    }
    if args.max_rows == Some(0) {
        return Err("max_rows must be at least 1 when supplied".into());
    }
    let root = require_bucket_root(&state.root, &args.bucket).map_err(|error| error.to_string())?;
    let dataset = VersionedDataset::open(root).map_err(|error| error.to_string())?;
    let schema = dataset.schema();

    for column in &args.select {
        if schema.column_index(column).is_none() {
            return Err(format!("unknown selected column {column}"));
        }
    }
    for filter in &args.filters {
        let (column, is_range) = match filter {
            QueryFilter::Eq { column, .. } => (column, false),
            QueryFilter::In { column, .. } => (column, false),
            QueryFilter::Range { column, .. } => (column, true),
        };
        let index = schema
            .column_index(column)
            .ok_or_else(|| format!("unknown filter column {column}"))?;
        if is_range
            && !matches!(
                schema.columns[index].logical_type,
                LogicalType::Unsigned | LogicalType::Signed
            )
        {
            return Err(format!(
                "range predicates require signed/unsigned column {column}"
            ));
        }
    }
    Ok(())
}

pub(super) fn create_export(
    state: &McpState,
    arguments: Value,
    public_base_url: &str,
) -> Result<ExportLink, String> {
    let args: ExportArgs =
        serde_json::from_value(arguments).map_err(|error| format!("invalid export arguments: {error}"))?;
    validate_args(state, &args)?;

    let now = now_epoch();
    let expires_at_epoch = now.saturating_add(args.expires_minutes.saturating_mul(60));
    let file_name = safe_file_name(args.file_name.as_deref(), &args.bucket, args.format);
    let token = random_token();
    let grant = ExportGrant {
        bucket: args.bucket.clone(),
        filters: args.filters,
        select: args.select,
        after_row_id: args.after_row_id,
        max_rows: args.max_rows,
        include_row_id: args.include_row_id,
        format: args.format,
        file_name: file_name.clone(),
        expires_at_epoch,
    };

    {
        let mut exports = state
            .exports
            .lock()
            .map_err(|_| "export registry lock poisoned".to_string())?;
        purge_expired(&mut exports, now);
        if exports.len() >= MAX_ACTIVE_EXPORTS {
            return Err(format!(
                "too many active export links; wait for an existing link to expire (limit {MAX_ACTIVE_EXPORTS})"
            ));
        }
        exports.insert(token.clone(), grant);
    }

    let base = public_base_url.trim_end_matches('/');
    let uri = format!("{base}/mcp/exports/{token}");
    let mime_type = args.format.mime_type().to_string();
    let structured = json!({
        "bucket":args.bucket,
        "format":args.format.as_str(),
        "file_name":file_name,
        "download_url":uri,
        "expires_at_epoch":expires_at_epoch,
        "expires_minutes":args.expires_minutes,
        "max_rows":args.max_rows,
        "include_row_id":args.include_row_id,
        "streamed":true
    });
    Ok(ExportLink {
        structured,
        uri,
        name: file_name,
        mime_type,
        description:
            "Short-lived streamed LHR export. Download it directly; bulk rows are not embedded in model context."
                .into(),
    })
}

pub(super) async fn download(
    State(state): State<McpState>,
    Path(token): Path<String>,
) -> Response {
    let now = now_epoch();
    let grant = {
        let mut exports = match state.exports.lock() {
            Ok(exports) => exports,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "export registry unavailable",
                )
                    .into_response();
            }
        };
        purge_expired(&mut exports, now);
        exports.get(&token).cloned()
    };
    let Some(grant) = grant else {
        return (
            StatusCode::NOT_FOUND,
            "export link is unknown or has expired",
        )
            .into_response();
    };

    let content_type = grant.format.mime_type();
    let file_name = grant.file_name.clone();
    let root = state.root.clone();
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(2);
    tokio::task::spawn_blocking(move || {
        if let Err(error) = stream_export(&root, &grant, &tx) {
            let _ = tx.blocking_send(Err(error));
        }
    });

    let body = Body::from_stream(ReceiverStream::new(rx));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(value) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{file_name}\""
    )) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn send_chunk(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    bytes: Vec<u8>,
) -> io::Result<bool> {
    if bytes.is_empty() {
        return Ok(true);
    }
    Ok(tx.blocking_send(Ok(Bytes::from(bytes))).is_ok())
}

fn stream_export(
    appliance_root: &FsPath,
    grant: &ExportGrant,
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
) -> io::Result<()> {
    let root = require_bucket_root(appliance_root, &grant.bucket)?;
    let dataset = VersionedDataset::open(root)?;
    let selected_columns: Vec<String> = if grant.select.is_empty() {
        dataset
            .schema()
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect()
    } else {
        grant.select.clone()
    };

    if matches!(grant.format, ExportFormat::Csv) {
        let mut writer = csv::WriterBuilder::new().from_writer(Vec::new());
        if grant.include_row_id {
            let mut header = Vec::with_capacity(selected_columns.len() + 1);
            header.push("_row_id".to_string());
            header.extend(selected_columns.iter().cloned());
            writer.write_record(header)?;
        } else {
            writer.write_record(&selected_columns)?;
        }
        let bytes = writer.into_inner().map_err(|error| error.into_error())?;
        if !send_chunk(tx, bytes)? {
            return Ok(());
        }
    }

    let mut after_row_id = grant.after_row_id;
    let mut remaining = grant.max_rows;
    loop {
        let page_limit = match remaining {
            Some(remaining) => EXPORT_PAGE_ROWS.min(remaining.min(usize::MAX as u64) as usize),
            None => EXPORT_PAGE_ROWS,
        };
        if page_limit == 0 {
            break;
        }
        let request = QueryRequest {
            filters: grant.filters.clone(),
            select: grant.select.clone(),
            limit: page_limit,
            after_row_id,
            max_rows_examined: None,
            timeout_ms: None,
        };
        let response = execute_query(&dataset, &request)?;
        if response.rows.is_empty() {
            break;
        }
        let returned = response.returned as u64;
        let next_cursor = response.next_cursor;

        let bytes = match grant.format {
            ExportFormat::Csv => encode_csv_rows(response.rows, grant.include_row_id)?,
            ExportFormat::Jsonl => encode_jsonl_rows(response.rows, grant.include_row_id)?,
        };
        if !send_chunk(tx, bytes)? {
            return Ok(());
        }

        if let Some(left) = &mut remaining {
            *left = left.saturating_sub(returned);
            if *left == 0 {
                break;
            }
        }
        let Some(cursor) = next_cursor else {
            break;
        };
        after_row_id = Some(cursor);
    }
    Ok(())
}

fn encode_csv_rows(
    rows: Vec<lhr::QueryApiRow>,
    include_row_id: bool,
) -> io::Result<Vec<u8>> {
    let mut writer = csv::WriterBuilder::new().from_writer(Vec::new());
    for row in rows {
        let mut record = Vec::with_capacity(row.values.len() + usize::from(include_row_id));
        if include_row_id {
            record.push(row.row_id.to_string());
        }
        record.extend(
            row.values
                .into_iter()
                .map(|value| value.value.unwrap_or_default()),
        );
        writer.write_record(record)?;
    }
    writer.into_inner().map_err(|error| error.into_error())
}

fn encode_jsonl_rows(
    rows: Vec<lhr::QueryApiRow>,
    include_row_id: bool,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for row in rows {
        let mut object = Map::new();
        if include_row_id {
            object.insert("_row_id".into(), Value::from(row.row_id));
        }
        for value in row.values {
            object.insert(
                value.column,
                value.value.map(Value::String).unwrap_or(Value::Null),
            );
        }
        serde_json::to_writer(&mut out, &Value::Object(object))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        out.push(b'\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_file_name_strips_paths_and_enforces_extension() {
        assert_eq!(
            safe_file_name(Some("../../weird report.csv"), "bucket", ExportFormat::Csv),
            "weird_report.csv"
        );
        assert_eq!(
            safe_file_name(Some("leads"), "bucket", ExportFormat::Jsonl),
            "leads.jsonl"
        );
    }

    #[test]
    fn export_tool_schema_does_not_impose_a_file_size_limit() {
        let schema = tool_schema();
        assert!(schema["properties"].get("max_bytes").is_none());
        assert_eq!(schema["properties"]["expires_minutes"]["maximum"], 1440);
    }
}

use crate::{
    add_index, apply_mutations_delta, compact_dataset, dataset_stats, dataset_status, drop_index,
    execute_query, import_csv, leased_generation_ids, list_generations, planner_indexes_for_request,
    rebuild_index, record_query, recover_catalog, resolve_dataset_root, vacuum_with_reader_leases,
    workload_report, CompactionConfig, CsvImportConfig, DatasetSchema, Mutation, MutationConfig,
    QueryRequest, VersionedDataset,
};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, OriginalUri, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{fs as tokio_fs, io::AsyncWriteExt, sync::{OwnedSemaphorePermit, Semaphore}};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRole { Read, Write, Admin }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceApiKey {
    pub id: String,
    pub token: String,
    pub role: ServiceRole,
}

fn default_bind() -> String { "127.0.0.1:8787".into() }
fn default_body_bytes() -> usize { 8 * 1024 * 1024 }
fn default_import_bytes() -> usize { 512 * 1024 * 1024 }
fn default_concurrency() -> usize { 64 }
fn default_rate_limit() -> u64 { 600 }
fn default_query_limit() -> usize { 10_000 }
fn default_rows_examined() -> u64 { 5_000_000 }
fn default_timeout_ms() -> u64 { 30_000 }
fn default_mutation_ops() -> usize { 100_000 }
fn default_batch_rows() -> usize { 65_536 }
fn default_sort_records() -> usize { 1_000_000 }
fn default_dictionary_bytes() -> usize { 256 * 1024 * 1024 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub api_keys: Vec<ServiceApiKey>,
    #[serde(default = "default_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_import_bytes")]
    pub max_import_bytes: usize,
    #[serde(default = "default_concurrency")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_minute: u64,
    #[serde(default = "default_query_limit")]
    pub max_query_limit: usize,
    #[serde(default = "default_rows_examined")]
    pub max_rows_examined: u64,
    #[serde(default = "default_timeout_ms")]
    pub max_query_timeout_ms: u64,
    #[serde(default = "default_mutation_ops")]
    pub max_mutation_ops: usize,
    #[serde(default = "default_batch_rows")]
    pub max_batch_rows: usize,
    #[serde(default = "default_sort_records")]
    pub max_sort_records: usize,
    #[serde(default = "default_dictionary_bytes")]
    pub max_dictionary_run_bytes: usize,
    /// A non-loopback HTTP bind is refused unless the operator explicitly confirms that TLS is
    /// terminated by a trusted reverse proxy or the listener lives on an equivalently protected
    /// private transport.
    #[serde(default)]
    pub behind_tls_proxy: bool,
    #[serde(default)]
    pub audit_log: Option<PathBuf>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(), api_keys: Vec::new(), max_body_bytes: default_body_bytes(),
            max_import_bytes: default_import_bytes(), max_concurrent_requests: default_concurrency(),
            rate_limit_per_minute: default_rate_limit(), max_query_limit: default_query_limit(),
            max_rows_examined: default_rows_examined(), max_query_timeout_ms: default_timeout_ms(),
            max_mutation_ops: default_mutation_ops(), max_batch_rows: default_batch_rows(),
            max_sort_records: default_sort_records(), max_dictionary_run_bytes: default_dictionary_bytes(),
            behind_tls_proxy: false, audit_log: None,
        }
    }
}

impl ServiceConfig {
    pub fn from_json_file(path: impl AsRef<Path>) -> io::Result<Self> {
        let config: Self = serde_json::from_slice(&fs::read(path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> io::Result<()> {
        let bind: SocketAddr = self.bind.parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid bind address: {e}")))?;
        if !bind.ip().is_loopback() && !self.behind_tls_proxy {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "refusing non-loopback cleartext bind; set behind_tls_proxy=true only when TLS/private transport is enforced upstream"));
        }
        if !bind.ip().is_loopback() && self.api_keys.is_empty() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "remote service listeners require at least one API key"));
        }
        if self.max_body_bytes == 0 || self.max_import_bytes == 0 || self.max_concurrent_requests == 0 || self.max_query_limit == 0
            || self.max_rows_examined == 0 || self.max_query_timeout_ms == 0 || self.max_mutation_ops == 0
            || self.max_batch_rows == 0 || self.max_sort_records == 0 || self.max_dictionary_run_bytes == 0
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "service resource ceilings must all be greater than zero"));
        }
        let mut ids = HashSet::new();
        let mut tokens = HashSet::new();
        for key in &self.api_keys {
            if key.id.trim().is_empty() || key.token.len() < 16 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "API key IDs must be non-empty and tokens must contain at least 16 characters"));
            }
            if !ids.insert(key.id.clone()) || !tokens.insert(hash_token(&key.token)) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "duplicate API key id or token"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Principal { id: String, role: ServiceRole }
#[derive(Debug)]
struct RateWindow { started: Instant, count: u64 }
#[derive(Default)]
struct RuntimeMetrics {
    requests: AtomicU64, errors: AtomicU64, active: AtomicU64, queries: AtomicU64,
    query_failures: AtomicU64, query_micros: AtomicU64, mutations: AtomicU64,
    compactions: AtomicU64, admin_actions: AtomicU64, rate_limited: AtomicU64, auth_failures: AtomicU64,
}
#[derive(Clone)]
struct ServiceState {
    root: PathBuf,
    config: Arc<ServiceConfig>,
    principals: Arc<HashMap<[u8; 32], Principal>>,
    rates: Arc<Mutex<HashMap<String, RateWindow>>>,
    semaphore: Arc<Semaphore>,
    metrics: Arc<RuntimeMetrics>,
    next_request_id: Arc<AtomicU64>,
}

fn hash_token(token: &str) -> [u8; 32] { Sha256::digest(token.as_bytes()).into() }
fn now_ms() -> u128 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() }

#[derive(Debug)]
struct ApiError { status: StatusCode, message: String, request_id: u64 }
impl ApiError {
    fn new(status: StatusCode, request_id: u64, message: impl Into<String>) -> Self { Self { status, request_id, message: message.into() } }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error":self.message,"request_id":self.request_id}))).into_response()
    }
}

struct RequestGuard { request_id: u64, actor: String, metrics: Arc<RuntimeMetrics>, _permit: OwnedSemaphorePermit }
impl Drop for RequestGuard { fn drop(&mut self) { self.metrics.active.fetch_sub(1, Ordering::Relaxed); } }

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

async fn begin_request(state: &ServiceState, headers: &HeaderMap, required: ServiceRole) -> Result<RequestGuard, ApiError> {
    let request_id = state.next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
    state.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let principal = if state.principals.is_empty() {
        Principal { id: "local-anonymous".into(), role: ServiceRole::Admin }
    } else {
        let Some(token) = bearer(headers) else {
            state.metrics.auth_failures.fetch_add(1, Ordering::Relaxed); state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, request_id, "missing bearer token"));
        };
        let Some(principal) = state.principals.get(&hash_token(token)).cloned() else {
            state.metrics.auth_failures.fetch_add(1, Ordering::Relaxed); state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, request_id, "invalid bearer token"));
        };
        principal
    };
    if principal.role < required {
        state.metrics.auth_failures.fetch_add(1, Ordering::Relaxed); state.metrics.errors.fetch_add(1, Ordering::Relaxed);
        return Err(ApiError::new(StatusCode::FORBIDDEN, request_id, "insufficient API-key role"));
    }
    if state.config.rate_limit_per_minute > 0 {
        let mut rates = state.rates.lock().map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, request_id, "rate limiter lock poisoned"))?;
        let now = Instant::now();
        let window = rates.entry(principal.id.clone()).or_insert(RateWindow { started: now, count: 0 });
        if now.duration_since(window.started).as_secs() >= 60 { window.started = now; window.count = 0; }
        if window.count >= state.config.rate_limit_per_minute {
            state.metrics.rate_limited.fetch_add(1, Ordering::Relaxed); state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, request_id, "API-key rate limit exceeded"));
        }
        window.count += 1;
    }
    let permit = state.semaphore.clone().try_acquire_owned().map_err(|_| {
        state.metrics.errors.fetch_add(1, Ordering::Relaxed);
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, request_id, "server concurrency limit reached")
    })?;
    state.metrics.active.fetch_add(1, Ordering::Relaxed);
    Ok(RequestGuard { request_id, actor: principal.id, metrics: state.metrics.clone(), _permit: permit })
}

#[derive(Debug, Serialize)]
struct AuditEvent<'a> { timestamp_ms: u128, request_id: u64, actor: &'a str, action: &'a str, success: bool, detail: Value }
fn append_audit(state: &ServiceState, event: &AuditEvent<'_>) -> io::Result<()> {
    let path = state.config.audit_log.clone().unwrap_or_else(|| state.root.join("audit").join("audit.jsonl"));
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    let mut file = OpenOptions::new().create(true).append(true).read(true).open(path)?;
    file.lock_exclusive()?;
    let result = (|| { serde_json::to_writer(&mut file, event).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?; file.write_all(b"\n")?; file.sync_data() })();
    let _ = FileExt::unlock(&file);
    result
}

fn io_status(error: &io::Error) -> StatusCode {
    match error.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => StatusCode::BAD_REQUEST,
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        io::ErrorKind::TimedOut => StatusCode::REQUEST_TIMEOUT,
        io::ErrorKind::WouldBlock => StatusCode::CONFLICT,
        io::ErrorKind::OutOfMemory => StatusCode::PAYLOAD_TOO_LARGE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
fn join_error(request_id: u64, error: tokio::task::JoinError) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, request_id, format!("blocking task failed: {error}"))
}

async fn healthz() -> impl IntoResponse { Json(json!({"status":"ok"})) }
async fn readyz(State(state): State<ServiceState>) -> Response {
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || VersionedDataset::open(root).map(|_| ())).await {
        Ok(Ok(())) => (StatusCode::OK, Json(json!({"status":"ready"}))).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"status":"not_ready"}))).into_response(),
    }
}

async fn query(State(state): State<ServiceState>, headers: HeaderMap, Json(mut request): Json<QueryRequest>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    if request.limit > state.config.max_query_limit {
        state.metrics.errors.fetch_add(1, Ordering::Relaxed);
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "query limit exceeds server maximum"));
    }
    request.max_rows_examined = Some(request.max_rows_examined.unwrap_or(state.config.max_rows_examined).min(state.config.max_rows_examined));
    request.timeout_ms = Some(request.timeout_ms.unwrap_or(state.config.max_query_timeout_ms).min(state.config.max_query_timeout_ms));
    let root = state.root.clone();
    let telemetry_root = state.root.clone();
    let request_copy = request.clone();
    state.metrics.queries.fetch_add(1, Ordering::Relaxed);
    let result = tokio::task::spawn_blocking(move || -> io::Result<_> {
        let dataset = VersionedDataset::open(&root)?;
        let indexes = planner_indexes_for_request(&dataset, &request_copy)?;
        let response = execute_query(&dataset, &request_copy)?;
        record_query(&telemetry_root, &request_copy, &response, indexes)?;
        Ok(response)
    }).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(response) => {
            state.metrics.query_micros.fetch_add(response.stats.elapsed_micros.min(u64::MAX as u128) as u64, Ordering::Relaxed);
            Ok(Json(json!({"request_id":guard.request_id,"result":response})))
        }
        Err(error) => {
            state.metrics.query_failures.fetch_add(1, Ordering::Relaxed); state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

fn csv_import_config(config: &ServiceConfig) -> CsvImportConfig {
    let defaults = CsvImportConfig::default();
    CsvImportConfig {
        page_rows: defaults.page_rows,
        batch_rows: defaults.batch_rows.min(config.max_batch_rows),
        max_sort_records: defaults.max_sort_records.min(config.max_sort_records),
        dictionary_run_bytes: defaults.dictionary_run_bytes.min(config.max_dictionary_run_bytes),
        accelerators: Vec::new(),
    }
}

async fn import_csv_upload(State(state): State<ServiceState>, headers: HeaderMap, mut multipart: Multipart) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let upload_dir = state.root.join("temp").join("studio-uploads");
    tokio_fs::create_dir_all(&upload_dir).await.map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
    let upload_path = upload_dir.join(format!("upload-{}.csv", guard.request_id));
    let mut schema: Option<DatasetSchema> = None;
    let mut uploaded = false;
    let mut uploaded_bytes = 0usize;

    while let Some(mut field) = multipart.next_field().await.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, format!("invalid multipart upload: {e}")))? {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "schema" => {
                let text = field.text().await.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, format!("invalid schema field: {e}")))?;
                let parsed: DatasetSchema = serde_json::from_str(&text).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, format!("invalid schema JSON: {e}")))?;
                parsed.validate().map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
                schema = Some(parsed);
            }
            "file" => {
                if uploaded {
                    let _ = tokio_fs::remove_file(&upload_path).await;
                    return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "upload contains more than one file"));
                }
                let mut output = tokio_fs::File::create(&upload_path).await.map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
                while let Some(chunk) = field.chunk().await.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, format!("failed reading upload: {e}")))? {
                    uploaded_bytes = uploaded_bytes.checked_add(chunk.len()).ok_or_else(|| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, guard.request_id, "upload size overflow"))?;
                    if uploaded_bytes > state.config.max_import_bytes {
                        drop(output);
                        let _ = tokio_fs::remove_file(&upload_path).await;
                        return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, guard.request_id, format!("CSV exceeds the {} byte Studio import limit", state.config.max_import_bytes)));
                    }
                    output.write_all(&chunk).await.map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
                }
                output.flush().await.map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
                uploaded = true;
            }
            _ => {}
        }
    }

    let Some(schema) = schema else {
        let _ = tokio_fs::remove_file(&upload_path).await;
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "missing schema field"));
    };
    if !uploaded || uploaded_bytes == 0 {
        let _ = tokio_fs::remove_file(&upload_path).await;
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "missing or empty CSV file"));
    }

    let root = state.root.clone();
    let import_path = upload_path.clone();
    let config = csv_import_config(&state.config);
    let result = tokio::task::spawn_blocking(move || import_csv(root, import_path, &schema, &config))
        .await.map_err(|e| join_error(guard.request_id, e))?;
    let _ = tokio_fs::remove_file(&upload_path).await;

    match result {
        Ok(report) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"import_csv", success:true, detail:json!({"bytes":uploaded_bytes,"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"import_csv", success:false, detail:json!({"bytes":uploaded_bytes,"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct WriteOptions { batch_rows: Option<usize>, max_sort_records: Option<usize>, dictionary_run_bytes: Option<usize> }
fn mutation_config(options: &WriteOptions, config: &ServiceConfig) -> io::Result<MutationConfig> {
    let defaults = MutationConfig::default();
    let batch_rows = options.batch_rows.unwrap_or(defaults.batch_rows);
    let max_sort_records = options.max_sort_records.unwrap_or(defaults.max_sort_records);
    let dictionary_run_bytes = options.dictionary_run_bytes.unwrap_or(defaults.dictionary_run_bytes);
    if batch_rows == 0 || batch_rows > config.max_batch_rows || max_sort_records == 0 || max_sort_records > config.max_sort_records || dictionary_run_bytes == 0 || dictionary_run_bytes > config.max_dictionary_run_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "mutation build options exceed service resource ceilings"));
    }
    Ok(MutationConfig { batch_rows, max_sort_records, dictionary_run_bytes })
}
fn compaction_config(options: &WriteOptions, config: &ServiceConfig) -> io::Result<CompactionConfig> {
    let defaults = CompactionConfig::default();
    let batch_rows = options.batch_rows.unwrap_or(defaults.batch_rows);
    let max_sort_records = options.max_sort_records.unwrap_or(defaults.max_sort_records);
    let dictionary_run_bytes = options.dictionary_run_bytes.unwrap_or(defaults.dictionary_run_bytes);
    if batch_rows == 0 || batch_rows > config.max_batch_rows || max_sort_records == 0 || max_sort_records > config.max_sort_records || dictionary_run_bytes == 0 || dictionary_run_bytes > config.max_dictionary_run_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "compaction options exceed service resource ceilings"));
    }
    Ok(CompactionConfig { batch_rows, max_sort_records, dictionary_run_bytes })
}

#[derive(Debug, Deserialize)]
struct MutationRequest { mutations: Vec<Mutation>, #[serde(default)] options: WriteOptions }
async fn mutate(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<MutationRequest>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Write).await?;
    if request.mutations.is_empty() || request.mutations.len() > state.config.max_mutation_ops {
        state.metrics.errors.fetch_add(1, Ordering::Relaxed);
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "mutation operation count is outside server limits"));
    }
    let config = mutation_config(&request.options, &state.config).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
    let root = state.root.clone();
    let operations = request.mutations;
    let count = operations.len();
    let result = tokio::task::spawn_blocking(move || apply_mutations_delta(root, &operations, &config)).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.mutations.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"mutate", success:true, detail:json!({"operations":count,"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"mutate", success:false, detail:json!({"operations":count,"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct CompactRequest { #[serde(default)] options: WriteOptions }
async fn compact(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<CompactRequest>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let config = compaction_config(&request.options, &state.config).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
    let root = state.root.clone();
    let result = tokio::task::spawn_blocking(move || compact_dataset(root, &config)).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.compactions.fetch_add(1, Ordering::Relaxed); state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"compact", success:true, detail:json!({"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"compact", success:false, detail:json!({"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

async fn stats(State(state): State<ServiceState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let root = state.root.clone();
    tokio::task::spawn_blocking(move || dataset_stats(root)).await.map_err(|e| join_error(guard.request_id, e))?
        .map(|value| Json(json!({"request_id":guard.request_id,"result":value}))).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))
}
async fn workload(State(state): State<ServiceState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let root = state.root.clone();
    tokio::task::spawn_blocking(move || workload_report(root)).await.map_err(|e| join_error(guard.request_id, e))?
        .map(|value| Json(json!({"request_id":guard.request_id,"result":value}))).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))
}
async fn generations(State(state): State<ServiceState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let root = state.root.clone();
    tokio::task::spawn_blocking(move || list_generations(root)).await.map_err(|e| join_error(guard.request_id, e))?
        .map(|value| Json(json!({"request_id":guard.request_id,"result":value}))).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))
}

#[derive(Debug, Deserialize)]
struct VacuumRequest { #[serde(default = "default_retain_generations")] retain: usize, #[serde(default)] protect: Vec<u64> }
fn default_retain_generations() -> usize { 2 }
async fn vacuum(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<VacuumRequest>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let root = state.root.clone();
    let result = tokio::task::spawn_blocking(move || vacuum_with_reader_leases(root, request.retain, &request.protect)).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"vacuum", success:true, detail:json!({"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"vacuum", success:false, detail:json!({"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}
async fn recover(State(state): State<ServiceState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let root = state.root.clone();
    let result = tokio::task::spawn_blocking(move || recover_catalog(root)).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"recover", success:true, detail:json!({"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"recover", success:false, detail:json!({"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

#[derive(Debug, Deserialize)]
struct IndexRequest { columns: Vec<String>, #[serde(default)] max_sort_records: Option<usize> }
async fn index_add(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<IndexRequest>) -> Result<Json<Value>, ApiError> { index_change(state, headers, request, "add").await }
async fn index_drop(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<IndexRequest>) -> Result<Json<Value>, ApiError> { index_change(state, headers, request, "drop").await }
async fn index_rebuild(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<IndexRequest>) -> Result<Json<Value>, ApiError> { index_change(state, headers, request, "rebuild").await }
async fn index_change(state: ServiceState, headers: HeaderMap, request: IndexRequest, action: &'static str) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    if request.columns.len() < 2 { return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "accelerator indexes require at least two columns")); }
    let max_sort = request.max_sort_records.unwrap_or(250_000);
    if max_sort == 0 || max_sort > state.config.max_sort_records { return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "max_sort_records exceeds service ceiling")); }
    let root = state.root.clone();
    let columns = request.columns.clone();
    let result = tokio::task::spawn_blocking(move || match action { "add" => add_index(root, &columns, max_sort), "drop" => drop_index(root, &columns), _ => rebuild_index(root, &columns, max_sort) }).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action, success:true, detail:json!({"columns":request.columns,"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action, success:false, detail:json!({"columns":request.columns,"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

fn process_metrics() -> Vec<(&'static str, u64)> {
    let mut out = Vec::new();
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:") {
                if let Some(kib) = value.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()) { out.push(("lhr_process_rss_bytes", kib.saturating_mul(1024))); }
            }
        }
    }
    if let Ok(stat) = fs::read_to_string("/proc/self/stat") {
        if let Some(end) = stat.rfind(')') {
            let fields: Vec<_> = stat[end + 1..].split_whitespace().collect();
            if let Some(value) = fields.get(7).and_then(|x| x.parse::<u64>().ok()) { out.push(("lhr_process_minor_page_faults_total", value)); }
            if let Some(value) = fields.get(9).and_then(|x| x.parse::<u64>().ok()) { out.push(("lhr_process_major_page_faults_total", value)); }
        }
    }
    if let Ok(io_text) = fs::read_to_string("/proc/self/io") {
        for line in io_text.lines() {
            if let Some(value) = line.strip_prefix("read_bytes:").and_then(|x| x.trim().parse::<u64>().ok()) { out.push(("lhr_process_read_bytes_total", value)); }
            if let Some(value) = line.strip_prefix("write_bytes:").and_then(|x| x.trim().parse::<u64>().ok()) { out.push(("lhr_process_write_bytes_total", value)); }
        }
    }
    out
}

async fn metrics(State(state): State<ServiceState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let mut lines = vec![
        format!("lhr_http_requests_total {}", state.metrics.requests.load(Ordering::Relaxed)),
        format!("lhr_http_errors_total {}", state.metrics.errors.load(Ordering::Relaxed)),
        format!("lhr_http_active_requests {}", state.metrics.active.load(Ordering::Relaxed)),
        format!("lhr_queries_total {}", state.metrics.queries.load(Ordering::Relaxed)),
        format!("lhr_query_failures_total {}", state.metrics.query_failures.load(Ordering::Relaxed)),
        format!("lhr_query_latency_microseconds_total {}", state.metrics.query_micros.load(Ordering::Relaxed)),
        format!("lhr_mutation_transactions_total {}", state.metrics.mutations.load(Ordering::Relaxed)),
        format!("lhr_compactions_total {}", state.metrics.compactions.load(Ordering::Relaxed)),
        format!("lhr_admin_actions_total {}", state.metrics.admin_actions.load(Ordering::Relaxed)),
        format!("lhr_rate_limited_total {}", state.metrics.rate_limited.load(Ordering::Relaxed)),
        format!("lhr_auth_failures_total {}", state.metrics.auth_failures.load(Ordering::Relaxed)),
    ];
    for (name, value) in process_metrics() { lines.push(format!("{name} {value}")); }
    if let Ok(ids) = leased_generation_ids(&state.root) { lines.push(format!("lhr_active_snapshot_generations {}", ids.len())); }
    if let Ok(resolved) = resolve_dataset_root(&state.root) {
        if let Ok(status) = dataset_status(resolved) {
            lines.push(format!("lhr_dataset_rows {}", status.rows));
            lines.push(format!("lhr_dataset_total_bytes {}", status.total_bytes));
            lines.push(format!("lhr_dataset_canonical_bytes {}", status.canonical_bytes));
            lines.push(format!("lhr_dataset_routing_bytes {}", status.routing_bytes));
        }
    }
    lines.push(String::new());
    let mut response = lines.join("\n").into_response();
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4"));
    drop(guard);
    Ok(response)
}

fn studio_not_found() -> Response { (StatusCode::NOT_FOUND, "not found").into_response() }
fn studio_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|x| x.to_str()).unwrap_or_default() {
        "html" => "text/html; charset=utf-8", "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8", "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml", "png" => "image/png", "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp", "ico" => "image/x-icon", "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}
async fn studio(OriginalUri(uri): OriginalUri) -> Response {
    let request_path = uri.path();
    if request_path.starts_with("/v1/") || request_path.starts_with("/metrics/") || request_path.starts_with("/healthz/") || request_path.starts_with("/readyz/") {
        return studio_not_found();
    }
    let Some(root) = env::var_os("LHR_STUDIO_DIR").map(PathBuf::from) else { return studio_not_found(); };
    let raw = request_path.trim_start_matches('/');
    let relative = if raw.is_empty() { Path::new("index.html") } else { Path::new(raw) };
    if relative.components().any(|component| !matches!(component, Component::Normal(_))) { return (StatusCode::BAD_REQUEST, "invalid asset path").into_response(); }
    let candidate = root.join(relative);
    let path = if candidate.is_file() { candidate } else { root.join("index.html") };
    if !path.is_file() { return studio_not_found(); }
    match fs::read(&path) {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(studio_content_type(&path)));
            let cache = if path.file_name().and_then(|x| x.to_str()) == Some("index.html") { "no-cache" } else if raw.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "public, max-age=3600" };
            response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            response
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "failed to read Studio asset").into_response(),
    }
}

pub async fn serve(root: impl AsRef<Path>, config: ServiceConfig) -> io::Result<()> {
    config.validate()?;
    let bind: SocketAddr = config.bind.parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut principals = HashMap::new();
    for key in &config.api_keys { principals.insert(hash_token(&key.token), Principal { id: key.id.clone(), role: key.role }); }
    let state = ServiceState {
        root: root.as_ref().to_path_buf(), semaphore: Arc::new(Semaphore::new(config.max_concurrent_requests)),
        config: Arc::new(config.clone()), principals: Arc::new(principals), rates: Arc::new(Mutex::new(HashMap::new())),
        metrics: Arc::new(RuntimeMetrics::default()), next_request_id: Arc::new(AtomicU64::new(0)),
    };
    // The control plane is allowed to start before a dataset exists. /readyz remains false and
    // data endpoints return ordinary errors until an initial generation is imported/published.
    let import_limit = config.max_import_bytes.saturating_add(1024 * 1024);
    let app = Router::new()
        .route("/healthz", get(healthz)).route("/readyz", get(readyz)).route("/metrics", get(metrics))
        .route("/v1/query", post(query)).route("/v1/stats", get(stats)).route("/v1/workload", get(workload))
        .route("/v1/generations", get(generations)).route("/v1/mutate", post(mutate))
        .route("/v1/admin/import/csv", post(import_csv_upload).layer(DefaultBodyLimit::max(import_limit)))
        .route("/v1/admin/compact", post(compact)).route("/v1/admin/vacuum", post(vacuum)).route("/v1/admin/recover", post(recover))
        .route("/v1/admin/index/add", post(index_add)).route("/v1/admin/index/drop", post(index_drop)).route("/v1/admin/index/rebuild", post(index_rebuild))
        .fallback(studio)
        .layer(DefaultBodyLimit::max(config.max_body_bytes)).with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await.map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_bind_requires_proxy_and_keys() {
        let mut config = ServiceConfig { bind: "0.0.0.0:8787".into(), ..Default::default() };
        assert!(config.validate().is_err()); config.behind_tls_proxy = true; assert!(config.validate().is_err());
        config.api_keys.push(ServiceApiKey { id: "reader".into(), token: "0123456789abcdef".into(), role: ServiceRole::Read });
        assert!(config.validate().is_ok());
    }
    #[test]
    fn duplicate_key_material_is_rejected() {
        let mut config = ServiceConfig::default();
        config.api_keys = vec![
            ServiceApiKey { id: "a".into(), token: "0123456789abcdef".into(), role: ServiceRole::Read },
            ServiceApiKey { id: "b".into(), token: "0123456789abcdef".into(), role: ServiceRole::Admin },
        ];
        assert!(config.validate().is_err());
    }
    #[test]
    fn role_order_matches_authorization_strength() {
        assert!(ServiceRole::Admin > ServiceRole::Write); assert!(ServiceRole::Write > ServiceRole::Read);
    }
    #[test]
    fn import_limit_must_be_nonzero() {
        let mut config = ServiceConfig::default();
        config.max_import_bytes = 0;
        assert!(config.validate().is_err());
    }
    #[test]
    fn studio_mime_types_are_stable() {
        assert_eq!(studio_content_type(Path::new("app.js")), "text/javascript; charset=utf-8");
        assert_eq!(studio_content_type(Path::new("app.css")), "text/css; charset=utf-8");
    }
}

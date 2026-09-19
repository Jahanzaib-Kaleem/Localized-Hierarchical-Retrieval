use crate::{
    add_index, append_csv_delta_with_progress, apply_mutations_delta, combine_buckets,
    compact_dataset, create_bucket, dataset_stats, dataset_status, delete_bucket, drop_index,
    execute_query, import_csv, import_csv_initial_with_progress, leased_generation_ids, list_buckets,
    list_generations, planner_indexes_for_request, read_schema, rebuild_index, record_query,
    recover_catalog, rename_bucket, require_bucket_root, resolve_dataset_root, transfer_rows,
    vacuum_with_reader_leases, workload_report, CompactionConfig, CsvImportConfig,
    CsvImportProgress, CsvImportStage, DatasetSchema, Mutation, MutationConfig, QueryRequest,
    VersionedDataset, DEFAULT_BUCKET,
};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Multipart, OriginalUri, Path as AxumPath, Query as AxumQuery, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
fn default_import_bytes() -> usize { 64usize * 1024 * 1024 * 1024 }
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
    import_locks: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

fn hash_token(token: &str) -> [u8; 32] { Sha256::digest(token.as_bytes()).into() }
fn now_ms() -> u128 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() }

fn default_bucket() -> String { DEFAULT_BUCKET.into() }

#[derive(Debug, Clone, Deserialize)]
struct BucketSelector {
    #[serde(default = "default_bucket")]
    bucket: String,
}

impl Default for BucketSelector {
    fn default() -> Self { Self { bucket: default_bucket() } }
}

#[derive(Debug, Clone, Deserialize)]
struct ServiceQueryRequest {
    #[serde(default = "default_bucket")]
    bucket: String,
    #[serde(flatten)]
    query: QueryRequest,
}


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

struct TempFileGuard { path: PathBuf }
impl TempFileGuard { fn new(path: PathBuf) -> Self { Self { path } } }
impl Drop for TempFileGuard { fn drop(&mut self) { let _ = fs::remove_file(&self.path); } }

const IMPORT_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const IMPORT_FINGERPRINT_BYTES: usize = 1024 * 1024;
const IMPORT_JOB_MAX_AGE_MS: u128 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ImportMode {
    Create,
    Append,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImportJob {
    id: String,
    bucket: String,
    mode: ImportMode,
    status: String,
    stage: String,
    file_name: String,
    #[serde(default)]
    file_fingerprint: Option<String>,
    bytes_received: u64,
    bytes_total: u64,
    rows_parsed: Option<u64>,
    result: Option<Value>,
    error: Option<String>,
    existing_dataset_preserved: bool,
    retry_safe: bool,
    updated_at_ms: u128,
    schema: DatasetSchema,
}

#[derive(Debug, Deserialize)]
struct ImportJobCreateRequest {
    #[serde(default = "default_bucket")]
    bucket: String,
    mode: ImportMode,
    schema: DatasetSchema,
    file_name: String,
    bytes_total: u64,
}

#[derive(Debug, Deserialize)]
struct ImportChunkQuery {
    offset: u64,
}

fn import_jobs_dir(root: &Path) -> PathBuf {
    root.join("temp").join("import-jobs")
}

fn import_uploads_dir(root: &Path) -> PathBuf {
    root.join("temp").join("studio-uploads")
}

fn valid_import_job_id(id: &str) -> bool {
    (1..=96).contains(&id.len())
        && id.starts_with("import-")
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn import_job_path(root: &Path, id: &str) -> io::Result<PathBuf> {
    if !valid_import_job_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid import job id"));
    }
    Ok(import_jobs_dir(root).join(format!("{id}.json")))
}

fn import_upload_path(root: &Path, id: &str) -> io::Result<PathBuf> {
    if !valid_import_job_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid import job id"));
    }
    Ok(import_uploads_dir(root).join(format!("{id}.csv")))
}

fn load_import_job(root: &Path, id: &str) -> io::Result<ImportJob> {
    let path = import_job_path(root, id)?;
    serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn save_import_job(root: &Path, job: &ImportJob) -> io::Result<()> {
    let dir = import_jobs_dir(root);
    fs::create_dir_all(&dir)?;
    let path = import_job_path(root, &job.id)?;
    let tmp = dir.join(format!(".{}.{}.tmp", job.id, std::process::id()));
    let bytes = serde_json::to_vec_pretty(job)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new().create(true).truncate(true).write(true).open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    let _ = fs::remove_file(&tmp);
    result
}

fn update_import_progress(root: &Path, job: &mut ImportJob, progress: CsvImportProgress) {
    job.status = "running".into();
    job.stage = match progress.stage {
        CsvImportStage::Validating => "validating",
        CsvImportStage::Parsing => "parsing",
        CsvImportStage::Building => "building",
        CsvImportStage::Indexing => "indexing",
        CsvImportStage::Publishing => "publishing",
    }
    .into();
    if progress.rows_parsed.is_some() {
        job.rows_parsed = progress.rows_parsed;
    }
    job.updated_at_ms = now_ms();
    let _ = save_import_job(root, job);
}

fn recover_import_jobs(root: &Path) -> io::Result<()> {
    let dir = import_jobs_dir(root);
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(mut job) = serde_json::from_slice::<ImportJob>(&fs::read(&path)?) else {
            continue;
        };
        let upload = import_upload_path(root, &job.id)?;
        match job.status.as_str() {
            "uploading" => {
                let stale = now_ms().saturating_sub(job.updated_at_ms) > IMPORT_JOB_MAX_AGE_MS;
                let length_matches = fs::metadata(&upload)
                    .map(|meta| meta.len() == job.bytes_received)
                    .unwrap_or(false);
                if stale || !length_matches {
                    job.status = "failed".into();
                    job.stage = "failed".into();
                    job.error = Some(if stale {
                        "incomplete upload expired before it was resumed".into()
                    } else {
                        "incomplete upload file does not match recorded progress".into()
                    });
                    job.existing_dataset_preserved = true;
                    job.retry_safe = true;
                    job.updated_at_ms = now_ms();
                    let _ = fs::remove_file(&upload);
                    let _ = save_import_job(root, &job);
                }
            }
            "queued" | "running" => {
                job.status = "failed".into();
                job.stage = "failed".into();
                job.error = Some(
                    "service restarted while the import was building; the bucket is atomic, but inspect the current generation before retrying because publication may have completed"
                        .into(),
                );
                job.existing_dataset_preserved = false;
                job.retry_safe = false;
                job.updated_at_ms = now_ms();
                let _ = fs::remove_file(&upload);
                let _ = save_import_job(root, &job);
            }
            "complete" | "failed" => {
                let _ = fs::remove_file(&upload);
            }
            _ => {}
        }
    }
    Ok(())
}

fn run_import_job(root: PathBuf, config: CsvImportConfig, id: String) {
    let Ok(mut job) = load_import_job(&root, &id) else {
        return;
    };
    job.status = "running".into();
    job.stage = "validating".into();
    job.updated_at_ms = now_ms();
    let _ = save_import_job(&root, &job);

    let upload = match import_upload_path(&root, &job.id) {
        Ok(path) => path,
        Err(error) => {
            job.status = "failed".into();
            job.stage = "failed".into();
            job.error = Some(error.to_string());
            job.existing_dataset_preserved = true;
            job.retry_safe = true;
            job.updated_at_ms = now_ms();
            let _ = save_import_job(&root, &job);
            return;
        }
    };
    let bucket_root = match require_bucket_root(&root, &job.bucket) {
        Ok(path) => path,
        Err(error) => {
            job.status = "failed".into();
            job.stage = "failed".into();
            job.error = Some(error.to_string());
            job.existing_dataset_preserved = true;
            job.retry_safe = true;
            job.updated_at_ms = now_ms();
            let _ = fs::remove_file(&upload);
            let _ = save_import_job(&root, &job);
            return;
        }
    };

    let schema = job.schema.clone();
    let result: io::Result<Value> = match job.mode {
        ImportMode::Create => import_csv_initial_with_progress(
            &bucket_root,
            &upload,
            &schema,
            &config,
            |progress| update_import_progress(&root, &mut job, progress),
        )
        .and_then(|report| {
            serde_json::to_value(report)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }),
        ImportMode::Append => append_csv_delta_with_progress(
            &bucket_root,
            &upload,
            &schema,
            &config,
            |progress| update_import_progress(&root, &mut job, progress),
        )
        .and_then(|report| {
            serde_json::to_value(report)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }),
    };

    match result {
        Ok(report) => {
            job.status = "complete".into();
            job.stage = "complete".into();
            job.result = Some(report);
            job.error = None;
            job.existing_dataset_preserved = true;
            job.retry_safe = false;
        }
        Err(error) => {
            job.status = "failed".into();
            job.stage = "failed".into();
            job.error = Some(error.to_string());
            job.existing_dataset_preserved = true;
            job.retry_safe = true;
        }
    }
    job.updated_at_ms = now_ms();
    let _ = fs::remove_file(&upload);
    let _ = save_import_job(&root, &job);
}


fn import_job_semaphore(state: &ServiceState, id: &str) -> io::Result<Arc<Semaphore>> {
    let mut locks = state
        .import_locks
        .lock()
        .map_err(|_| io::Error::other("import job lock map poisoned"))?;
    Ok(locks
        .entry(id.to_owned())
        .or_insert_with(|| Arc::new(Semaphore::new(1)))
        .clone())
}

async fn cleanup_expired_import_jobs(state: &ServiceState) {
    let dir = import_jobs_dir(&state.root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let now = now_ms();
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = entry
            .path()
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let Ok(semaphore) = import_job_semaphore(state, &id) else {
            continue;
        };
        let Ok(_permit) = semaphore.acquire_owned().await else {
            continue;
        };
        let Ok(mut job) = load_import_job(&state.root, &id) else {
            continue;
        };
        if job.status == "uploading"
            && now.saturating_sub(job.updated_at_ms) > IMPORT_JOB_MAX_AGE_MS
        {
            job.status = "failed".into();
            job.stage = "failed".into();
            job.error = Some("incomplete upload expired before it was resumed".into());
            job.existing_dataset_preserved = true;
            job.retry_safe = true;
            job.updated_at_ms = now;
            if let Ok(upload) = import_upload_path(&state.root, &id) {
                let _ = fs::remove_file(upload);
            }
            let _ = save_import_job(&state.root, &job);
        } else if matches!(job.status.as_str(), "complete" | "failed") {
            if let Ok(upload) = import_upload_path(&state.root, &id) {
                let _ = fs::remove_file(upload);
            }
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

async fn begin_request_inner(
    state: &ServiceState,
    headers: &HeaderMap,
    required: ServiceRole,
    enforce_rate_limit: bool,
) -> Result<RequestGuard, ApiError> {
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
    if enforce_rate_limit && state.config.rate_limit_per_minute > 0 {
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

async fn begin_request(
    state: &ServiceState,
    headers: &HeaderMap,
    required: ServiceRole,
) -> Result<RequestGuard, ApiError> {
    begin_request_inner(state, headers, required, true).await
}

async fn begin_upload_chunk_request(
    state: &ServiceState,
    headers: &HeaderMap,
) -> Result<RequestGuard, ApiError> {
    // Upload chunks are admin-only, fixed-size, sequential-offset constrained, aggregate-size
    // capped, disk-preflighted, and still subject to the global concurrency ceiling. Counting each
    // 4 MiB chunk against the ordinary request-per-minute bucket would make multi-GB ingestion
    // fail merely because the transport is intentionally chunked.
    begin_request_inner(state, headers, ServiceRole::Admin, false).await
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
        io::ErrorKind::WouldBlock | io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
        io::ErrorKind::OutOfMemory => StatusCode::PAYLOAD_TOO_LARGE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
fn join_error(request_id: u64, error: tokio::task::JoinError) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, request_id, format!("blocking task failed: {error}"))
}

fn selected_bucket_root(state: &ServiceState, bucket: &str, request_id: u64) -> Result<PathBuf, ApiError> {
    require_bucket_root(&state.root, bucket)
        .map_err(|error| ApiError::new(io_status(&error), request_id, error.to_string()))
}


async fn healthz() -> impl IntoResponse { Json(json!({"status":"ok"})) }
async fn readyz(State(state): State<ServiceState>) -> Response {
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || list_buckets(root)).await {
        Ok(Ok(buckets)) if buckets.iter().any(|bucket| bucket.ready) => {
            (StatusCode::OK, Json(json!({"status":"ready"}))).into_response()
        }
        _ => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"status":"not_ready"}))).into_response(),
    }
}

async fn query(State(state): State<ServiceState>, headers: HeaderMap, Json(payload): Json<ServiceQueryRequest>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let bucket = payload.bucket;
    let mut request = payload.query;
    if request.limit > state.config.max_query_limit {
        state.metrics.errors.fetch_add(1, Ordering::Relaxed);
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "query limit exceeds server maximum"));
    }
    request.max_rows_examined = Some(request.max_rows_examined.unwrap_or(state.config.max_rows_examined).min(state.config.max_rows_examined));
    request.timeout_ms = Some(request.timeout_ms.unwrap_or(state.config.max_query_timeout_ms).min(state.config.max_query_timeout_ms));
    let root = selected_bucket_root(&state, &bucket, guard.request_id)?;
    let telemetry_root = root.clone();
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
            Ok(Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":response})))
        }
        Err(error) => {
            state.metrics.query_failures.fetch_add(1, Ordering::Relaxed); state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

async fn dataset_schema(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    AxumQuery(selector): AxumQuery<BucketSelector>,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let root = selected_bucket_root(&state, &selector.bucket, guard.request_id)?;
    let result = tokio::task::spawn_blocking(move || {
        let resolved = resolve_dataset_root(root)?;
        read_schema(resolved)
    })
    .await
    .map_err(|error| join_error(guard.request_id, error))?
    .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    Ok(Json(json!({"request_id":guard.request_id,"bucket":selector.bucket,"result":result})))
}

async fn import_job_create(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    Json(request): Json<ImportJobCreateRequest>,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    if request.bytes_total == 0 {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "CSV file is empty"));
    }
    let max_import = u64::try_from(state.config.max_import_bytes).unwrap_or(u64::MAX);
    if request.bytes_total > max_import {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            guard.request_id,
            format!("CSV exceeds the {} byte import limit", state.config.max_import_bytes),
        ));
    }
    request
        .schema
        .validate()
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    let available = fs2::available_space(&state.root)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    let disk_floor = request
        .bytes_total
        .saturating_mul(4)
        .saturating_add(64 * 1024 * 1024);
    if disk_floor > available {
        return Err(ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            guard.request_id,
            format!(
                "insufficient free space for a safe import build: need at least {disk_floor} bytes available for upload/build staging, found {available}"
            ),
        ));
    }
    let bucket_root = selected_bucket_root(&state, &request.bucket, guard.request_id)?;
    match (request.mode, resolve_dataset_root(&bucket_root)) {
        (ImportMode::Create, Ok(_)) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                guard.request_id,
                "bucket is already initialized; use append",
            ));
        }
        (ImportMode::Append, Err(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                guard.request_id,
                "bucket is empty; create its initial dataset first",
            ));
        }
        (_, Err(error)) if error.kind() != io::ErrorKind::NotFound => {
            return Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()));
        }
        _ => {}
    }

    let id = format!("import-{}-{}", now_ms(), guard.request_id);
    let uploads = import_uploads_dir(&state.root);
    let jobs = import_jobs_dir(&state.root);
    tokio_fs::create_dir_all(&uploads)
        .await
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    tokio_fs::create_dir_all(&jobs)
        .await
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    let upload = import_upload_path(&state.root, &id)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    tokio_fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&upload)
        .await
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;

    let file_name = Path::new(&request.file_name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload.csv")
        .chars()
        .take(240)
        .collect::<String>();
    let job = ImportJob {
        id,
        bucket: request.bucket,
        mode: request.mode,
        status: "uploading".into(),
        stage: "uploading".into(),
        file_name,
        file_fingerprint: None,
        bytes_received: 0,
        bytes_total: request.bytes_total,
        rows_parsed: None,
        result: None,
        error: None,
        existing_dataset_preserved: true,
        retry_safe: true,
        updated_at_ms: now_ms(),
        schema: request.schema,
    };
    if let Err(error) = save_import_job(&state.root, &job) {
        let _ = tokio_fs::remove_file(&upload).await;
        return Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()));
    }
    Ok(Json(json!({"request_id":guard.request_id,"result":job})))
}

async fn import_job_chunk(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    AxumQuery(query): AxumQuery<ImportChunkQuery>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_upload_chunk_request(&state, &headers).await?;
    let job_semaphore = import_job_semaphore(&state, &id)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    let _job_permit = job_semaphore
        .acquire_owned()
        .await
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, guard.request_id, "import job lock closed"))?;
    if body.is_empty() || body.len() > IMPORT_CHUNK_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            guard.request_id,
            format!("import chunks must contain 1..={IMPORT_CHUNK_BYTES} bytes"),
        ));
    }
    let mut job = load_import_job(&state.root, &id)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    if job.status != "uploading" {
        return Err(ApiError::new(StatusCode::CONFLICT, guard.request_id, "import is no longer accepting upload chunks"));
    }
    if query.offset == 0 {
        let required = usize::try_from(job.bytes_total.min(IMPORT_FINGERPRINT_BYTES as u64))
            .unwrap_or(IMPORT_FINGERPRINT_BYTES);
        if body.len() < required {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                guard.request_id,
                format!("first import chunk must contain at least {required} bytes for fingerprint validation"),
            ));
        }
        let digest = Sha256::digest(&body[..required]);
        let actual = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        match job.file_fingerprint.as_deref() {
            Some(expected) if expected != actual => {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    guard.request_id,
                    "selected file does not match the import job fingerprint",
                ));
            }
            None if job.bytes_received == 0 => {
                job.file_fingerprint = Some(actual);
            }
            None => {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    guard.request_id,
                    "upload progress exists without a durable file fingerprint",
                ));
            }
            Some(_) => {}
        }
    }
    if query.offset < job.bytes_received {
        return Ok(Json(json!({"request_id":guard.request_id,"result":job})));
    }
    if query.offset != job.bytes_received {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            guard.request_id,
            format!("chunk offset mismatch: server expects {}", job.bytes_received),
        ));
    }

    let chunk_len = u64::try_from(body.len()).unwrap_or(u64::MAX);
    let next = job
        .bytes_received
        .checked_add(chunk_len)
        .ok_or_else(|| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, guard.request_id, "upload size overflow"))?;
    if next > job.bytes_total {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "chunk exceeds declared CSV size"));
    }

    let upload = import_upload_path(&state.root, &id)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    let mut output = tokio_fs::OpenOptions::new()
        .append(true)
        .write(true)
        .open(&upload)
        .await
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    let actual = output
        .metadata()
        .await
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?
        .len();
    if actual != job.bytes_received {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            guard.request_id,
            "upload file length does not match persisted job progress",
        ));
    }
    if let Err(error) = output.write_all(&body).await {
        let _ = output.set_len(job.bytes_received).await;
        return Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()));
    }
    if let Err(error) = output.flush().await {
        let _ = output.set_len(job.bytes_received).await;
        return Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()));
    }
    if let Err(error) = output.sync_data().await {
        let _ = output.set_len(job.bytes_received).await;
        return Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()));
    }

    job.bytes_received = next;
    job.updated_at_ms = now_ms();
    if let Err(error) = save_import_job(&state.root, &job) {
        let _ = output.set_len(query.offset).await;
        let _ = output.sync_data().await;
        return Err(ApiError::new(
            io_status(&error),
            guard.request_id,
            format!("failed to persist upload progress; chunk was rolled back: {error}"),
        ));
    }
    Ok(Json(json!({"request_id":guard.request_id,"result":job})))
}

async fn import_job_complete(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let job_semaphore = import_job_semaphore(&state, &id)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    let _job_permit = job_semaphore
        .acquire_owned()
        .await
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, guard.request_id, "import job lock closed"))?;
    let mut job = load_import_job(&state.root, &id)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    if job.status != "uploading" {
        return Err(ApiError::new(StatusCode::CONFLICT, guard.request_id, "import is not waiting for upload completion"));
    }
    if job.bytes_received != job.bytes_total {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            guard.request_id,
            format!(
                "upload is incomplete: received {} of {} bytes",
                job.bytes_received, job.bytes_total
            ),
        ));
    }
    let upload = import_upload_path(&state.root, &id)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    let actual = tokio_fs::metadata(&upload)
        .await
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?
        .len();
    if actual != job.bytes_total {
        return Err(ApiError::new(StatusCode::CONFLICT, guard.request_id, "uploaded file size does not match declared size"));
    }

    job.status = "queued".into();
    job.stage = "queued".into();
    job.retry_safe = false;
    job.updated_at_ms = now_ms();
    save_import_job(&state.root, &job)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;

    let root = state.root.clone();
    let config = csv_import_config(&state.config);
    let job_id = id.clone();
    tokio::task::spawn_blocking(move || run_import_job(root, config, job_id));
    Ok(Json(json!({"request_id":guard.request_id,"result":job})))
}

async fn import_job_status(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let job = load_import_job(&state.root, &id)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    Ok(Json(json!({"request_id":guard.request_id,"result":job})))
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
    let _upload_cleanup = TempFileGuard::new(upload_path.clone());
    let mut schema: Option<DatasetSchema> = None;
    let mut bucket = default_bucket();
    let mut uploaded = false;
    let mut uploaded_bytes = 0usize;

    while let Some(mut field) = multipart.next_field().await.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, format!("invalid multipart upload: {e}")))? {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "bucket" => {
                bucket = field.text().await.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, format!("invalid bucket field: {e}")))?;
            }
            "schema" => {
                let text = field.text().await.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, format!("invalid schema field: {e}")))?;
                let parsed: DatasetSchema = serde_json::from_str(&text).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, format!("invalid schema JSON: {e}")))?;
                parsed.validate().map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
                schema = Some(parsed);
            }
            "file" => {
                if uploaded {
                    return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "upload contains more than one file"));
                }
                let mut output = tokio_fs::File::create(&upload_path).await.map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
                while let Some(chunk) = field.chunk().await.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, format!("failed reading upload: {e}")))? {
                    uploaded_bytes = uploaded_bytes.checked_add(chunk.len()).ok_or_else(|| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, guard.request_id, "upload size overflow"))?;
                    if uploaded_bytes > state.config.max_import_bytes {
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
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "missing schema field"));
    };
    if !uploaded || uploaded_bytes == 0 {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "missing or empty CSV file"));
    }

    let root = selected_bucket_root(&state, &bucket, guard.request_id)?;
    let import_path = upload_path.clone();
    let config = csv_import_config(&state.config);
    let result = tokio::task::spawn_blocking(move || import_csv(root, import_path, &schema, &config))
        .await.map_err(|e| join_error(guard.request_id, e))?;

    match result {
        Ok(report) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"import_csv", success:true, detail:json!({"bucket":bucket,"bytes":uploaded_bytes,"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"import_csv", success:false, detail:json!({"bucket":bucket,"bytes":uploaded_bytes,"error":error.to_string()}) });
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
struct MutationRequest {
    #[serde(default = "default_bucket")]
    bucket: String,
    mutations: Vec<Mutation>,
    #[serde(default)]
    options: WriteOptions,
}
async fn mutate(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<MutationRequest>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Write).await?;
    if request.mutations.is_empty() || request.mutations.len() > state.config.max_mutation_ops {
        state.metrics.errors.fetch_add(1, Ordering::Relaxed);
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "mutation operation count is outside server limits"));
    }
    let config = mutation_config(&request.options, &state.config).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
    let root = selected_bucket_root(&state, &request.bucket, guard.request_id)?;
    let bucket = request.bucket.clone();
    let operations = request.mutations;
    let count = operations.len();
    let result = tokio::task::spawn_blocking(move || apply_mutations_delta(root, &operations, &config)).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.mutations.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"mutate", success:true, detail:json!({"bucket":bucket,"operations":count,"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"mutate", success:false, detail:json!({"bucket":bucket,"operations":count,"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct CompactRequest {
    #[serde(default = "default_bucket")]
    bucket: String,
    #[serde(default)]
    options: WriteOptions,
}
async fn compact(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<CompactRequest>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let config = compaction_config(&request.options, &state.config).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))?;
    let bucket = request.bucket.clone();
    let root = selected_bucket_root(&state, &bucket, guard.request_id)?;
    let result = tokio::task::spawn_blocking(move || compact_dataset(root, &config)).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.compactions.fetch_add(1, Ordering::Relaxed); state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"compact", success:true, detail:json!({"bucket":bucket,"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"compact", success:false, detail:json!({"bucket":bucket,"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

async fn stats(State(state): State<ServiceState>, headers: HeaderMap, AxumQuery(selector): AxumQuery<BucketSelector>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let bucket = selector.bucket;
    let root = selected_bucket_root(&state, &bucket, guard.request_id)?;
    tokio::task::spawn_blocking(move || dataset_stats(root)).await.map_err(|e| join_error(guard.request_id, e))?
        .map(|value| Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":value}))).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))
}
async fn workload(State(state): State<ServiceState>, headers: HeaderMap, AxumQuery(selector): AxumQuery<BucketSelector>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let bucket = selector.bucket;
    let root = selected_bucket_root(&state, &bucket, guard.request_id)?;
    tokio::task::spawn_blocking(move || workload_report(root)).await.map_err(|e| join_error(guard.request_id, e))?
        .map(|value| Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":value}))).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))
}
async fn generations(State(state): State<ServiceState>, headers: HeaderMap, AxumQuery(selector): AxumQuery<BucketSelector>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let bucket = selector.bucket;
    let root = selected_bucket_root(&state, &bucket, guard.request_id)?;
    tokio::task::spawn_blocking(move || list_generations(root)).await.map_err(|e| join_error(guard.request_id, e))?
        .map(|value| Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":value}))).map_err(|e| ApiError::new(io_status(&e), guard.request_id, e.to_string()))
}

#[derive(Debug, Deserialize)]
struct VacuumRequest {
    #[serde(default = "default_bucket")]
    bucket: String,
    #[serde(default = "default_retain_generations")]
    retain: usize,
    #[serde(default)]
    protect: Vec<u64>,
}
fn default_retain_generations() -> usize { 2 }
async fn vacuum(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<VacuumRequest>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let bucket = request.bucket.clone();
    let root = selected_bucket_root(&state, &bucket, guard.request_id)?;
    let result = tokio::task::spawn_blocking(move || vacuum_with_reader_leases(root, request.retain, &request.protect)).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"vacuum", success:true, detail:json!({"bucket":bucket,"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"vacuum", success:false, detail:json!({"bucket":bucket,"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}
async fn recover(State(state): State<ServiceState>, headers: HeaderMap, AxumQuery(selector): AxumQuery<BucketSelector>) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let bucket = selector.bucket;
    let root = selected_bucket_root(&state, &bucket, guard.request_id)?;
    let result = tokio::task::spawn_blocking(move || recover_catalog(root)).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"recover", success:true, detail:json!({"bucket":bucket,"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action:"recover", success:false, detail:json!({"bucket":bucket,"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

#[derive(Debug, Deserialize)]
struct IndexRequest {
    #[serde(default = "default_bucket")]
    bucket: String,
    columns: Vec<String>,
    #[serde(default)]
    max_sort_records: Option<usize>,
}
async fn index_add(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<IndexRequest>) -> Result<Json<Value>, ApiError> { index_change(state, headers, request, "add").await }
async fn index_drop(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<IndexRequest>) -> Result<Json<Value>, ApiError> { index_change(state, headers, request, "drop").await }
async fn index_rebuild(State(state): State<ServiceState>, headers: HeaderMap, Json(request): Json<IndexRequest>) -> Result<Json<Value>, ApiError> { index_change(state, headers, request, "rebuild").await }
async fn index_change(state: ServiceState, headers: HeaderMap, request: IndexRequest, action: &'static str) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    if request.columns.len() < 2 { return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "accelerator indexes require at least two columns")); }
    let max_sort = request.max_sort_records.unwrap_or(250_000);
    if max_sort == 0 || max_sort > state.config.max_sort_records { return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "max_sort_records exceeds service ceiling")); }
    let bucket = request.bucket.clone();
    let root = selected_bucket_root(&state, &bucket, guard.request_id)?;
    let columns = request.columns.clone();
    let result = tokio::task::spawn_blocking(move || match action { "add" => add_index(root, &columns, max_sort), "drop" => drop_index(root, &columns), _ => rebuild_index(root, &columns, max_sort) }).await.map_err(|e| join_error(guard.request_id, e))?;
    match result {
        Ok(report) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action, success:true, detail:json!({"bucket":bucket,"columns":request.columns,"report":report}) });
            Ok(Json(json!({"request_id":guard.request_id,"bucket":bucket,"result":report})))
        }
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent { timestamp_ms:now_ms(), request_id:guard.request_id, actor:&guard.actor, action, success:false, detail:json!({"bucket":bucket,"columns":request.columns,"error":error.to_string()}) });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}


async fn buckets(State(state): State<ServiceState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Read).await?;
    let root = state.root.clone();
    tokio::task::spawn_blocking(move || list_buckets(root))
        .await
        .map_err(|error| join_error(guard.request_id, error))?
        .map(|value| Json(json!({"request_id":guard.request_id,"result":value})))
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))
}

#[derive(Debug, Deserialize)]
struct BucketCreateRequest { id: String, name: String }

async fn bucket_create(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    Json(request): Json<BucketCreateRequest>,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let root = state.root.clone();
    let id = request.id.clone();
    let name = request.name.clone();
    let result = tokio::task::spawn_blocking(move || create_bucket(root, &id, &name))
        .await
        .map_err(|error| join_error(guard.request_id, error))?;
    match result {
        Ok(bucket) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent {
                timestamp_ms: now_ms(), request_id: guard.request_id, actor: &guard.actor,
                action: "bucket_create", success: true,
                detail: json!({"id":request.id,"name":request.name}),
            });
            Ok(Json(json!({"request_id":guard.request_id,"result":bucket})))
        }
        Err(error) => Err(ApiError::new(io_status(&error), guard.request_id, error.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct BucketRenameRequest { id: String, name: String }

async fn bucket_rename(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    Json(request): Json<BucketRenameRequest>,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    let root = state.root.clone();
    let id = request.id.clone();
    let name = request.name.clone();
    let result = tokio::task::spawn_blocking(move || rename_bucket(root, &id, &name))
        .await
        .map_err(|error| join_error(guard.request_id, error))?;
    match result {
        Ok(bucket) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent {
                timestamp_ms: now_ms(), request_id: guard.request_id, actor: &guard.actor,
                action: "bucket_rename", success: true,
                detail: json!({"id":request.id,"name":request.name}),
            });
            Ok(Json(json!({"request_id":guard.request_id,"result":bucket})))
        }
        Err(error) => Err(ApiError::new(io_status(&error), guard.request_id, error.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct BucketDeleteRequest { id: String, confirm: String }

async fn bucket_delete(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    Json(request): Json<BucketDeleteRequest>,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    if request.confirm != request.id {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            guard.request_id,
            "confirm must exactly match the bucket id",
        ));
    }
    let root = state.root.clone();
    let id = request.id.clone();
    let result = tokio::task::spawn_blocking(move || delete_bucket(root, &id))
        .await
        .map_err(|error| join_error(guard.request_id, error))?;
    match result {
        Ok(()) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent {
                timestamp_ms: now_ms(), request_id: guard.request_id, actor: &guard.actor,
                action: "bucket_delete", success: true, detail: json!({"id":request.id}),
            });
            Ok(Json(json!({"request_id":guard.request_id,"result":{"deleted":request.id}})))
        }
        Err(error) => Err(ApiError::new(io_status(&error), guard.request_id, error.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct BucketCombineRequest {
    sources: Vec<String>,
    target_id: String,
    target_name: String,
}

async fn bucket_combine(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    Json(request): Json<BucketCombineRequest>,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Admin).await?;
    if request.sources.len() > 64 {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, guard.request_id, "at most 64 source buckets may be combined at once"));
    }
    let root = state.root.clone();
    let sources = request.sources.clone();
    let target_id = request.target_id.clone();
    let target_name = request.target_name.clone();
    let config = csv_import_config(&state.config);
    let result = tokio::task::spawn_blocking(move || {
        combine_buckets(root, &sources, &target_id, &target_name, &config)
    })
    .await
    .map_err(|error| join_error(guard.request_id, error))?;
    match result {
        Ok(report) => {
            state.metrics.admin_actions.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent {
                timestamp_ms: now_ms(), request_id: guard.request_id, actor: &guard.actor,
                action: "bucket_combine", success: true,
                detail: json!({"sources":request.sources,"target_id":request.target_id,"report":report}),
            });
            Ok(Json(json!({"request_id":guard.request_id,"result":report})))
        }
        Err(error) => {
            let _ = append_audit(&state, &AuditEvent {
                timestamp_ms: now_ms(), request_id: guard.request_id, actor: &guard.actor,
                action: "bucket_combine", success: false,
                detail: json!({"sources":request.sources,"target_id":request.target_id,"error":error.to_string()}),
            });
            Err(ApiError::new(io_status(&error), guard.request_id, error.to_string()))
        }
    }
}

#[derive(Debug, Deserialize)]
struct BucketTransferRequest {
    source: String,
    destination: String,
    row_ids: Vec<u64>,
    #[serde(default)]
    move_rows: bool,
}

async fn bucket_transfer(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    Json(request): Json<BucketTransferRequest>,
) -> Result<Json<Value>, ApiError> {
    let guard = begin_request(&state, &headers, ServiceRole::Write).await?;
    if request.row_ids.is_empty() || request.row_ids.len() > state.config.max_mutation_ops {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            guard.request_id,
            format!("row transfer count must be 1..={}", state.config.max_mutation_ops),
        ));
    }
    let root = state.root.clone();
    let source = request.source.clone();
    let destination = request.destination.clone();
    let row_ids = request.row_ids.clone();
    let move_rows = request.move_rows;
    let config = mutation_config(&WriteOptions::default(), &state.config)
        .map_err(|error| ApiError::new(io_status(&error), guard.request_id, error.to_string()))?;
    let result = tokio::task::spawn_blocking(move || {
        transfer_rows(root, &source, &destination, &row_ids, move_rows, &config)
    })
    .await
    .map_err(|error| join_error(guard.request_id, error))?;
    match result {
        Ok(report) => {
            state.metrics.mutations.fetch_add(1, Ordering::Relaxed);
            let _ = append_audit(&state, &AuditEvent {
                timestamp_ms: now_ms(), request_id: guard.request_id, actor: &guard.actor,
                action: "bucket_transfer", success: true,
                detail: json!({"source":request.source,"destination":request.destination,"rows":request.row_ids.len(),"move":request.move_rows,"report":report}),
            });
            Ok(Json(json!({"request_id":guard.request_id,"result":report})))
        }
        Err(error) => Err(ApiError::new(io_status(&error), guard.request_id, error.to_string())),
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
    if let Ok(buckets) = list_buckets(&state.root) {
        let ready = buckets.iter().filter(|bucket| bucket.ready).count();
        let rows = buckets.iter().map(|bucket| bucket.rows).sum::<u64>();
        let bytes = buckets.iter().map(|bucket| bucket.total_bytes).sum::<u64>();
        lines.push(format!("lhr_buckets {}", buckets.len()));
        lines.push(format!("lhr_ready_buckets {}", ready));
        lines.push(format!("lhr_bucket_rows_total {}", rows));
        lines.push(format!("lhr_bucket_storage_bytes_total {}", bytes));
    }
    if let Ok(ids) = leased_generation_ids(&state.root) { lines.push(format!("lhr_active_snapshot_generations {}", ids.len())); }
    // Legacy unlabeled dataset gauges remain the reserved default bucket for compatibility.
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
    recover_import_jobs(root.as_ref())?;
    let bind: SocketAddr = config.bind.parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut principals = HashMap::new();
    for key in &config.api_keys { principals.insert(hash_token(&key.token), Principal { id: key.id.clone(), role: key.role }); }
    let state = ServiceState {
        root: root.as_ref().to_path_buf(), semaphore: Arc::new(Semaphore::new(config.max_concurrent_requests)),
        config: Arc::new(config.clone()), principals: Arc::new(principals), rates: Arc::new(Mutex::new(HashMap::new())),
        metrics: Arc::new(RuntimeMetrics::default()), next_request_id: Arc::new(AtomicU64::new(0)),
        import_locks: Arc::new(Mutex::new(HashMap::new())),
    };
    // The control plane is allowed to start before a dataset exists. /readyz remains false and
    // data endpoints return ordinary errors until an initial generation is imported/published.
    let import_limit = config.max_import_bytes.saturating_add(1024 * 1024);
    let app = Router::new()
        .route("/healthz", get(healthz)).route("/readyz", get(readyz)).route("/metrics", get(metrics))
        .route("/v1/buckets", get(buckets))
        .route("/v1/admin/buckets/create", post(bucket_create))
        .route("/v1/admin/buckets/rename", post(bucket_rename))
        .route("/v1/admin/buckets/delete", post(bucket_delete))
        .route("/v1/admin/buckets/combine", post(bucket_combine))
        .route("/v1/buckets/transfer", post(bucket_transfer))
        .route("/v1/query", post(query)).route("/v1/stats", get(stats)).route("/v1/schema", get(dataset_schema)).route("/v1/workload", get(workload))
        .route("/v1/generations", get(generations)).route("/v1/mutate", post(mutate))
        .route("/v1/admin/import/csv", post(import_csv_upload).layer(DefaultBodyLimit::max(import_limit)))
        .route("/v1/admin/imports", post(import_job_create))
        .route("/v1/admin/imports/{id}", get(import_job_status))
        .route("/v1/admin/imports/{id}/chunk", put(import_job_chunk).layer(DefaultBodyLimit::max(IMPORT_CHUNK_BYTES)))
        .route("/v1/admin/imports/{id}/complete", post(import_job_complete))
        .route("/v1/admin/compact", post(compact)).route("/v1/admin/vacuum", post(vacuum)).route("/v1/admin/recover", post(recover))
        .route("/v1/admin/index/add", post(index_add)).route("/v1/admin/index/drop", post(index_drop)).route("/v1/admin/index/rebuild", post(index_rebuild))
        .fallback(studio)
        .layer(DefaultBodyLimit::max(config.max_body_bytes)).with_state(state.clone());
    let cleanup_state = state.clone();
    let cleanup_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
            cleanup_expired_import_jobs(&cleanup_state).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; })
        .await
        .map_err(io::Error::other);
    cleanup_task.abort();
    result
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
    fn default_import_ceiling_allows_large_multi_part_csvs() {
        assert!(ServiceConfig::default().max_import_bytes > 737 * 1024 * 1024);
        assert_eq!(IMPORT_CHUNK_BYTES, 4 * 1024 * 1024);
    }
    #[test]
    fn import_job_ids_reject_path_traversal() {
        assert!(valid_import_job_id("import-123-4"));
        assert!(!valid_import_job_id("../import-123"));
        assert!(!valid_import_job_id("import-123/4"));
    }
    #[test]
    fn temp_file_guard_removes_file_on_drop() {
        let path = env::temp_dir().join(format!("lhr-service-upload-{}-{}.csv", std::process::id(), now_ms()));
        fs::write(&path, b"test").unwrap();
        {
            let _guard = TempFileGuard::new(path.clone());
            assert!(path.is_file());
        }
        assert!(!path.exists());
    }
    #[test]
    fn studio_mime_types_are_stable() {
        assert_eq!(studio_content_type(Path::new("app.js")), "text/javascript; charset=utf-8");
        assert_eq!(studio_content_type(Path::new("app.css")), "text/css; charset=utf-8");
    }
}

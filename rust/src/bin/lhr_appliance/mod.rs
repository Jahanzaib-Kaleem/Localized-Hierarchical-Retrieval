mod export;
mod tools;

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use lhr::{ServiceConfig, ServiceRole};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MCP_CURRENT: &str = "2026-07-28";
pub const MCP_LEGACY: &str = "2025-11-25";
const MCP_PROTOCOL_HEADER: &str = "mcp-protocol-version";
const MCP_METHOD_HEADER: &str = "mcp-method";
const MCP_NAME_HEADER: &str = "mcp-name";

#[derive(Clone)]
struct Principal {
    id: String,
    role: ServiceRole,
}

struct RateWindow {
    started: Instant,
    count: u64,
}

#[derive(Default)]
pub(super) struct McpCounters {
    pub requests: AtomicU64,
    pub failures: AtomicU64,
    pub tool_calls: AtomicU64,
    pub auth_failures: AtomicU64,
    pub rate_limited: AtomicU64,
}

#[derive(Clone)]
pub(super) struct McpState {
    pub root: PathBuf,
    pub config: Arc<ServiceConfig>,
    principals: Arc<HashMap<[u8; 32], Principal>>,
    rates: Arc<Mutex<HashMap<String, RateWindow>>>,
    semaphore: Arc<Semaphore>,
    pub counters: Arc<McpCounters>,
    pub exports: export::ExportRegistry,
    pub export_downloads: Arc<Semaphore>,
}

struct RequestGuard {
    actor: String,
    role: ServiceRole,
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

pub struct McpServer {
    state: McpState,
    bind: SocketAddr,
    max_body_bytes: usize,
}

impl McpServer {
    pub fn new(root: PathBuf, config: ServiceConfig, bind: SocketAddr) -> io::Result<Self> {
        let mut principals = HashMap::new();
        for key in &config.api_keys {
            principals.insert(
                hash_token(&key.token),
                Principal {
                    id: key.id.clone(),
                    role: key.role,
                },
            );
        }
        let max_body_bytes = config.max_body_bytes;
        let max_concurrent_requests = config.max_concurrent_requests;
        Ok(Self {
            state: McpState {
                root,
                config: Arc::new(config),
                principals: Arc::new(principals),
                rates: Arc::new(Mutex::new(HashMap::new())),
                semaphore: Arc::new(Semaphore::new(max_concurrent_requests)),
                counters: Arc::new(McpCounters::default()),
                exports: export::new_registry(),
                export_downloads: Arc::new(Semaphore::new(2)),
            },
            bind,
            max_body_bytes,
        })
    }

    pub async fn serve(self) -> io::Result<()> {
        let app = Router::new()
            .route("/healthz", get(health))
            .route("/mcp", post(mcp))
            .route("/mcp/exports/{token}", get(export::download))
            .route("/exports/{token}", get(export::download))
            .layer(DefaultBodyLimit::max(self.max_body_bytes))
            .with_state(self.state);
        let listener = tokio::net::TcpListener::bind(self.bind).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(io::Error::other)
    }
}

fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn authorize(state: &McpState, headers: &HeaderMap) -> Result<RequestGuard, Response> {
    state.counters.requests.fetch_add(1, Ordering::Relaxed);
    let principal = if state.principals.is_empty() {
        Principal {
            id: "local-anonymous".into(),
            role: ServiceRole::Admin,
        }
    } else {
        let Some(token) = bearer(headers) else {
            state.counters.auth_failures.fetch_add(1, Ordering::Relaxed);
            state.counters.failures.fetch_add(1, Ordering::Relaxed);
            return Err((StatusCode::UNAUTHORIZED, "missing bearer token").into_response());
        };
        let Some(principal) = state.principals.get(&hash_token(token)).cloned() else {
            state.counters.auth_failures.fetch_add(1, Ordering::Relaxed);
            state.counters.failures.fetch_add(1, Ordering::Relaxed);
            return Err((StatusCode::UNAUTHORIZED, "invalid bearer token").into_response());
        };
        principal
    };

    if state.config.rate_limit_per_minute > 0 {
        let mut rates = state
            .rates
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "rate limiter unavailable").into_response())?;
        let now = Instant::now();
        let window = rates.entry(principal.id.clone()).or_insert(RateWindow {
            started: now,
            count: 0,
        });
        if now.duration_since(window.started).as_secs() >= 60 {
            window.started = now;
            window.count = 0;
        }
        if window.count >= state.config.rate_limit_per_minute {
            state.counters.rate_limited.fetch_add(1, Ordering::Relaxed);
            state.counters.failures.fetch_add(1, Ordering::Relaxed);
            return Err((StatusCode::TOO_MANY_REQUESTS, "MCP rate limit exceeded").into_response());
        }
        window.count += 1;
    }

    let permit = state
        .semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "MCP concurrency limit reached").into_response())?;
    Ok(RequestGuard {
        actor: principal.id,
        role: principal.role,
        _permit: permit,
    })
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.into()}})
}

fn server_meta() -> Value {
    json!({"name":"lhr","version":env!("CARGO_PKG_VERSION")})
}

fn stamp_modern(mut result: Value, modern: bool) -> Value {
    if !modern {
        return result;
    }
    if let Some(object) = result.as_object_mut() {
        let meta = object.entry("_meta").or_insert_with(|| json!({}));
        if let Some(meta) = meta.as_object_mut() {
            meta.insert("io.modelcontextprotocol/serverInfo".into(), server_meta());
        }
    }
    result
}

fn header_text<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn forwarded_value<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    header_text(headers, name)?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn public_base_url(headers: &HeaderMap, behind_tls_proxy: bool) -> Option<String> {
    if let Ok(configured) = std::env::var("LHR_PUBLIC_BASE_URL") {
        let configured = configured.trim().trim_end_matches('/');
        if (configured.starts_with("https://") || configured.starts_with("http://"))
            && !configured.chars().any(char::is_whitespace)
        {
            return Some(configured.to_owned());
        }
    }

    let host = forwarded_value(headers, "x-forwarded-host")
        .or_else(|| header_text(headers, "host").map(str::trim))?;
    if host.is_empty()
        || host.chars().any(char::is_whitespace)
        || host.contains('/')
        || host.contains('\\')
        || host.contains('?')
        || host.contains('#')
    {
        return None;
    }
    let proto = forwarded_value(headers, "x-forwarded-proto")
        .unwrap_or(if behind_tls_proxy { "https" } else { "http" });
    if proto != "https" && proto != "http" {
        return None;
    }
    Some(format!("{proto}://{host}"))
}

fn validate_modern_headers(headers: &HeaderMap, request: &RpcRequest) -> Result<bool, String> {
    let version = header_text(headers, MCP_PROTOCOL_HEADER);
    let modern = version == Some(MCP_CURRENT);
    if !modern {
        return Ok(false);
    }
    let method = header_text(headers, MCP_METHOD_HEADER)
        .ok_or_else(|| "modern MCP requests require Mcp-Method".to_string())?;
    if method != request.method {
        return Err("Mcp-Method does not match JSON-RPC method".into());
    }
    if request.method == "tools/call" {
        let body_name = request.params.get("name").and_then(Value::as_str).unwrap_or_default();
        let header_name = header_text(headers, MCP_NAME_HEADER)
            .ok_or_else(|| "tools/call requires Mcp-Name".to_string())?;
        if body_name != header_name {
            return Err("Mcp-Name does not match tools/call params.name".into());
        }
    }
    Ok(true)
}

fn discover_result() -> Value {
    json!({
        "supportedVersions":[MCP_CURRENT,MCP_LEGACY],
        "capabilities":{"tools":{"listChanged":false}},
        "instructions":"LHR is an exact structured database. Prefer bounded cursor-paginated lhr_query calls for small lead retrieval. Use lhr_export for downloadable large result sets instead of paging bulk data into model context. Diagnostics and benchmark tools expose real storage/process behavior; write/admin tools are role-gated.",
        "ttlMs":30000,
        "cacheScope":"private"
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion":MCP_LEGACY,
        "capabilities":{"tools":{"listChanged":false}},
        "serverInfo":server_meta(),
        "instructions":"LHR is an exact structured database. Prefer bounded cursor-paginated lhr_query calls for small retrieval and lhr_export for large downloadable result sets."
    })
}

async fn health() -> impl IntoResponse {
    Json(json!({"status":"ok","protocol":MCP_CURRENT,"legacy":MCP_LEGACY}))
}

async fn mcp(
    State(state): State<McpState>,
    headers: HeaderMap,
    Json(request): Json<RpcRequest>,
) -> Response {
    let id = request.id.clone().unwrap_or(Value::Null);
    if request.jsonrpc.as_deref().is_some_and(|value| value != "2.0") {
        return (StatusCode::BAD_REQUEST, Json(rpc_error(id, -32600, "jsonrpc must be 2.0"))).into_response();
    }
    let modern = match validate_modern_headers(&headers, &request) {
        Ok(modern) => modern,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(rpc_error(id, -32020, message))).into_response();
        }
    };

    if request.method.starts_with("notifications/") {
        // Legacy initialized notifications carry no response body. Modern MCP defines no
        // client-to-server notifications, but accepting an empty notification is harmless.
        return StatusCode::ACCEPTED.into_response();
    }

    let guard = match authorize(&state, &headers).await {
        Ok(guard) => guard,
        Err(response) => return response,
    };

    let result = match request.method.as_str() {
        "server/discover" => rpc_ok(id, stamp_modern(discover_result(), true)),
        "initialize" => rpc_ok(id, initialize_result()),
        "ping" => rpc_ok(id, stamp_modern(json!({}), modern)),
        "tools/list" => rpc_ok(
            id,
            stamp_modern(
                json!({
                    "tools":tools::tool_catalog(guard.role),
                    "ttlMs":30000,
                    "cacheScope":"private"
                }),
                modern,
            ),
        ),
        "resources/list" => rpc_ok(
            id,
            stamp_modern(json!({"resources":[],"ttlMs":30000,"cacheScope":"private"}), modern),
        ),
        "prompts/list" => rpc_ok(
            id,
            stamp_modern(json!({"prompts":[],"ttlMs":30000,"cacheScope":"private"}), modern),
        ),
        "tools/call" => {
            let params: ToolCallParams = match serde_json::from_value(request.params) {
                Ok(value) => value,
                Err(error) => {
                    return (StatusCode::BAD_REQUEST, Json(rpc_error(id, -32602, format!("invalid tools/call params: {error}")))).into_response();
                }
            };
            let Some(required) = tools::required_role(&params.name) else {
                return Json(rpc_error(id, -32601, format!("unknown tool {}", params.name))).into_response();
            };
            if guard.role < required {
                state.counters.auth_failures.fetch_add(1, Ordering::Relaxed);
                rpc_error(id, -32003, "insufficient API-key role for tool")
            } else {
                state.counters.tool_calls.fetch_add(1, Ordering::Relaxed);
                let state_copy = state.clone();
                let actor = guard.actor.clone();
                let role = guard.role;
                let tool_name = params.name;
                let arguments = params.arguments;
                let base_url = public_base_url(&headers, state.config.behind_tls_proxy);
                match tokio::task::spawn_blocking(move || {
                    tools::call_tool(
                        &state_copy,
                        &actor,
                        role,
                        &tool_name,
                        arguments,
                        base_url.as_deref(),
                    )
                })
                .await
                {
                    Ok(Ok(value)) => rpc_ok(id, stamp_modern(tools::tool_ok(value), modern)),
                    Ok(Err(message)) => {
                        state.counters.failures.fetch_add(1, Ordering::Relaxed);
                        rpc_ok(id, stamp_modern(tools::tool_error(message), modern))
                    }
                    Err(error) => {
                        state.counters.failures.fetch_add(1, Ordering::Relaxed);
                        rpc_ok(id, stamp_modern(tools::tool_error(format!("MCP worker failed: {error}")), modern))
                    }
                }
            }
        }
        _ => rpc_error(id, -32601, format!("unsupported method {}", request.method)),
    };

    let mut response = Json(result).into_response();
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if modern {
        response.headers_mut().insert(
            HeaderName::from_static(MCP_PROTOCOL_HEADER),
            HeaderValue::from_static(MCP_CURRENT),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_order_controls_tool_visibility() {
        let read = tools::tool_catalog(ServiceRole::Read);
        let names: Vec<_> = read.iter().filter_map(|tool| tool.get("name").and_then(Value::as_str)).collect();
        assert!(names.contains(&"lhr_query"));
        assert!(!names.contains(&"lhr_mutate"));
        assert!(!names.contains(&"lhr_compact"));
    }

    #[test]
    fn discovery_advertises_both_supported_eras() {
        let result = discover_result();
        assert_eq!(result["supportedVersions"][0], MCP_CURRENT);
        assert_eq!(result["supportedVersions"][1], MCP_LEGACY);
    }
}

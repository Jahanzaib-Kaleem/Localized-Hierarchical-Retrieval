use super::McpState;
use fs2::FileExt;
use lhr::{
    add_index, apply_mutations_delta, combine_buckets, compact_dataset, create_bucket, dataset_stats,
    dataset_status, delete_bucket, execute_query, leased_generation_ids, list_buckets,
    list_generations, list_indexes, planner_indexes_for_request, read_schema, rebuild_index,
    record_query, recover_catalog, rename_bucket, require_bucket_root, resolve_dataset_root,
    transfer_rows, verify_versioned_dataset, vacuum_with_reader_leases, workload_report,
    CompactionConfig, CsvImportConfig, LogicalPredicate, Mutation, MutationConfig, QueryRequest,
    ServiceRole, VersionedDataset, DEFAULT_BUCKET,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn annotation(read_only: bool, destructive: bool, idempotent: bool) -> Value {
    json!({
        "readOnlyHint":read_only,
        "destructiveHint":destructive,
        "idempotentHint":idempotent,
        "openWorldHint":false
    })
}

fn default_bucket() -> String { DEFAULT_BUCKET.into() }

fn bucket_only_schema() -> Value {
    json!({
        "type":"object",
        "properties":{"bucket":{"type":"string","default":"default"}},
        "additionalProperties":false
    })
}

fn empty_schema() -> Value {
    json!({"type":"object","properties":{},"additionalProperties":false})
}

fn query_schema() -> Value {
    json!({
        "type":"object",
        "required":["filters"],
        "properties":{
            "bucket":{"type":"string","default":"default"},
            "filters":{"type":"array","items":{
                "type":"object","required":["op","column"],
                "properties":{
                    "op":{"type":"string","enum":["eq","in","range"]},
                    "column":{"type":"string"},
                    "value":{"type":["string","null"]},
                    "values":{"type":"array","items":{"type":["string","null"]}},
                    "gte":{"type":["string","null"]},
                    "lte":{"type":["string","null"]}
                }
            }},
            "select":{"type":"array","items":{"type":"string"}},
            "limit":{"type":"integer","minimum":1},
            "after_row_id":{"type":["integer","null"],"minimum":0},
            "max_rows_examined":{"type":["integer","null"],"minimum":1},
            "timeout_ms":{"type":["integer","null"],"minimum":1}
        },
        "additionalProperties":false
    })
}

fn index_tool(name: &str, title: &str, description: &str, destructive: bool) -> Value {
    json!({
        "name":name,"title":title,"description":description,
        "inputSchema":{
            "type":"object","required":["columns"],
            "properties":{
                "bucket":{"type":"string","default":"default"},
                "columns":{"type":"array","minItems":2,"items":{"type":"string"}},
                "max_sort_records":{"type":"integer","minimum":1}
            },
            "additionalProperties":false
        },
        "annotations":annotation(false,destructive,false)
    })
}

fn all_tools() -> Vec<(ServiceRole, Value)> {
    vec![
        (ServiceRole::Read, json!({
            "name":"lhr_query","title":"Query LHR",
            "description":"Run a bounded exact typed query. Use cursor pagination for lead retrieval; equality predicates use LHR exact indexes when available.",
            "inputSchema":query_schema(),"annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_row","title":"Fetch row",
            "description":"Fetch one visible logical row by stable row ID.",
            "inputSchema":{"type":"object","required":["row_id"],"properties":{"row_id":{"type":"integer","minimum":0}},"additionalProperties":false},
            "annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_schema","title":"Schema",
            "description":"Return the current logical schema including types, nullability, normalization and null literals.",
            "inputSchema":empty_schema(),"annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_stats","title":"Dataset statistics",
            "description":"Return row/page counts, canonical/routing/total bytes, column cardinalities and index metadata.",
            "inputSchema":empty_schema(),"annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_explain","title":"Explain exact route",
            "description":"Explain equality routing across base and delta layers without changing data.",
            "inputSchema":{"type":"object","required":["predicates"],"properties":{"predicates":{"type":"array","minItems":1,"items":{"type":"object","required":["column"],"properties":{"column":{"type":"string"},"value":{"type":["string","null"]}},"additionalProperties":false}}},"additionalProperties":false},
            "annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_workload","title":"Workload telemetry",
            "description":"Return observed query shapes, latency percentiles, index usage and workload-based accelerator recommendations.",
            "inputSchema":empty_schema(),"annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_generations","title":"Generations",
            "description":"List immutable generations, CURRENT state and actively leased generations.",
            "inputSchema":empty_schema(),"annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_indexes","title":"Indexes",
            "description":"List current exact/routing indexes and their representation/storage metadata.",
            "inputSchema":empty_schema(),"annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_diagnostics","title":"Runtime diagnostics",
            "description":"Inspect process RSS, page faults, disk I/O, filesystem capacity, MCP counters and current dataset storage state.",
            "inputSchema":empty_schema(),"annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_benchmark_query","title":"Benchmark query",
            "description":"Benchmark one bounded query repeatedly on a pinned immutable snapshot and report min/median/p95/p99/max latency plus RSS/page-fault deltas. Benchmark samples are not written into workload telemetry.",
            "inputSchema":{"type":"object","required":["request"],"properties":{"request":query_schema(),"iterations":{"type":"integer","minimum":1,"maximum":50,"default":10},"warmup":{"type":"integer","minimum":0,"maximum":20,"default":2}},"additionalProperties":false},
            "annotations":annotation(true,false,true)
        })),
        (ServiceRole::Read, json!({
            "name":"lhr_verify","title":"Verify dataset",
            "description":"Run structural and versioned integrity verification. This is read-only but can be disk-intensive on very large datasets.",
            "inputSchema":empty_schema(),"annotations":annotation(true,false,true)
        })),
        (ServiceRole::Write, json!({
            "name":"lhr_mutate","title":"Mutate rows",
            "description":"Apply insert/update/delete operations as one crash-safe immutable delta transaction.",
            "inputSchema":{"type":"object","required":["mutations"],"properties":{"mutations":{"type":"array","minItems":1,"items":{"type":"object"}},"options":{"type":"object","properties":{"batch_rows":{"type":"integer","minimum":1},"max_sort_records":{"type":"integer","minimum":1},"dictionary_run_bytes":{"type":"integer","minimum":1}},"additionalProperties":false}},"additionalProperties":false},
            "annotations":annotation(false,true,false)
        })),
        (ServiceRole::Admin, json!({
            "name":"lhr_compact","title":"Compact dataset",
            "description":"Merge immutable delta layers into a clean base generation using bounded-memory compaction.",
            "inputSchema":{"type":"object","properties":{"options":{"type":"object","properties":{"batch_rows":{"type":"integer","minimum":1},"max_sort_records":{"type":"integer","minimum":1},"dictionary_run_bytes":{"type":"integer","minimum":1}},"additionalProperties":false}},"additionalProperties":false},
            "annotations":annotation(false,true,false)
        })),
        (ServiceRole::Admin, json!({
            "name":"lhr_vacuum","title":"Vacuum generations",
            "description":"Remove old unleased generations while preserving CURRENT, retained and explicitly protected generations.",
            "inputSchema":{"type":"object","properties":{"retain":{"type":"integer","minimum":1,"default":2},"protect":{"type":"array","items":{"type":"integer","minimum":1}}},"additionalProperties":false},
            "annotations":annotation(false,true,false)
        })),
        (ServiceRole::Admin, json!({
            "name":"lhr_recover","title":"Recover catalog",
            "description":"Verify published generations and repoint CURRENT to the newest fully valid generation when recovery is required.",
            "inputSchema":empty_schema(),"annotations":annotation(false,true,false)
        })),
        (ServiceRole::Admin, index_tool("lhr_index_add","Add accelerator","Add an exact multi-column accelerator index.",false)),
        (ServiceRole::Admin, index_tool("lhr_index_drop","Drop accelerator","Drop an exact multi-column accelerator while retaining the singleton correctness backbone.",true)),
        (ServiceRole::Admin, index_tool("lhr_index_rebuild","Rebuild accelerator","Rebuild an exact multi-column accelerator in a new immutable generation.",false)),
    ]
}

pub(super) fn tool_catalog(role: ServiceRole) -> Vec<Value> {
    all_tools()
        .into_iter()
        .filter(|(required, _)| role >= *required)
        .map(|(_, tool)| tool)
        .collect()
}

pub(super) fn required_role(name: &str) -> Option<ServiceRole> {
    all_tools().into_iter().find_map(|(role, tool)| {
        (tool.get("name").and_then(Value::as_str) == Some(name)).then_some(role)
    })
}

pub(super) fn tool_ok(value: Value) -> Value {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
    json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":false})
}

pub(super) fn tool_error(message: impl Into<String>) -> Value {
    let message = message.into();
    json!({"content":[{"type":"text","text":message}],"isError":true})
}

#[derive(Debug, Deserialize)]
struct RowArgs {
    row_id: u64,
}

#[derive(Debug, Deserialize)]
struct EqPredicateArg {
    column: String,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExplainArgs {
    predicates: Vec<EqPredicateArg>,
}

#[derive(Debug, Deserialize, Default)]
struct WriteOptions {
    batch_rows: Option<usize>,
    max_sort_records: Option<usize>,
    dictionary_run_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct MutationArgs {
    mutations: Vec<Mutation>,
    #[serde(default)]
    options: WriteOptions,
}

#[derive(Debug, Deserialize, Default)]
struct CompactArgs {
    #[serde(default)]
    options: WriteOptions,
}

#[derive(Debug, Deserialize)]
struct VacuumArgs {
    #[serde(default = "default_retain")]
    retain: usize,
    #[serde(default)]
    protect: Vec<u64>,
}

fn default_retain() -> usize {
    2
}

#[derive(Debug, Deserialize)]
struct IndexArgs {
    columns: Vec<String>,
    #[serde(default)]
    max_sort_records: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct BenchmarkArgs {
    request: QueryRequest,
    #[serde(default = "default_iterations")]
    iterations: usize,
    #[serde(default = "default_warmup")]
    warmup: usize,
}

fn default_iterations() -> usize {
    10
}
fn default_warmup() -> usize {
    2
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn bounded_query(mut request: QueryRequest, state: &McpState) -> Result<QueryRequest, String> {
    if request.limit == 0 || request.limit > state.config.max_query_limit {
        return Err(format!("query limit must be 1..={}", state.config.max_query_limit));
    }
    request.max_rows_examined = Some(
        request
            .max_rows_examined
            .unwrap_or(state.config.max_rows_examined)
            .min(state.config.max_rows_examined),
    );
    request.timeout_ms = Some(
        request
            .timeout_ms
            .unwrap_or(state.config.max_query_timeout_ms)
            .min(state.config.max_query_timeout_ms),
    );
    Ok(request)
}

fn mutation_config(options: &WriteOptions, state: &McpState) -> Result<MutationConfig, String> {
    let defaults = MutationConfig::default();
    let batch_rows = options.batch_rows.unwrap_or(defaults.batch_rows);
    let max_sort_records = options.max_sort_records.unwrap_or(defaults.max_sort_records);
    let dictionary_run_bytes = options.dictionary_run_bytes.unwrap_or(defaults.dictionary_run_bytes);
    if batch_rows == 0
        || batch_rows > state.config.max_batch_rows
        || max_sort_records == 0
        || max_sort_records > state.config.max_sort_records
        || dictionary_run_bytes == 0
        || dictionary_run_bytes > state.config.max_dictionary_run_bytes
    {
        return Err("mutation build options exceed service resource ceilings".into());
    }
    Ok(MutationConfig {
        batch_rows,
        max_sort_records,
        dictionary_run_bytes,
    })
}

fn compaction_config(options: &WriteOptions, state: &McpState) -> Result<CompactionConfig, String> {
    let defaults = CompactionConfig::default();
    let batch_rows = options.batch_rows.unwrap_or(defaults.batch_rows);
    let max_sort_records = options.max_sort_records.unwrap_or(defaults.max_sort_records);
    let dictionary_run_bytes = options.dictionary_run_bytes.unwrap_or(defaults.dictionary_run_bytes);
    if batch_rows == 0
        || batch_rows > state.config.max_batch_rows
        || max_sort_records == 0
        || max_sort_records > state.config.max_sort_records
        || dictionary_run_bytes == 0
        || dictionary_run_bytes > state.config.max_dictionary_run_bytes
    {
        return Err("compaction options exceed service resource ceilings".into());
    }
    Ok(CompactionConfig {
        batch_rows,
        max_sort_records,
        dictionary_run_bytes,
    })
}

fn audit(state: &McpState, actor: &str, action: &str, success: bool, detail: Value) {
    let path = state.root.join("audit").join("mcp-audit.jsonl");
    let _ = (|| -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        file.lock_exclusive()?;
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        serde_json::to_writer(
            &mut file,
            &json!({"timestamp_ms":timestamp_ms,"actor":actor,"action":action,"success":success,"detail":detail}),
        )
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        FileExt::unlock(&file)?;
        Ok(())
    })();
}

fn mutating<T, F>(
    state: &McpState,
    actor: &str,
    action: &str,
    detail: Value,
    operation: F,
) -> Result<Value, String>
where
    T: serde::Serialize,
    F: FnOnce() -> io::Result<T>,
{
    match operation() {
        Ok(report) => {
            let value = serde_json::to_value(report).map_err(|error| error.to_string())?;
            audit(
                state,
                actor,
                action,
                true,
                json!({"request":detail,"report":value.clone()}),
            );
            Ok(value)
        }
        Err(error) => {
            audit(
                state,
                actor,
                action,
                false,
                json!({"request":detail,"error":error.to_string()}),
            );
            Err(error.to_string())
        }
    }
}

pub(super) fn call_tool(
    state: &McpState,
    actor: &str,
    role: ServiceRole,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let required = required_role(name).ok_or_else(|| format!("unknown MCP tool {name}"))?;
    if role < required {
        return Err("insufficient API-key role for tool".into());
    }

    match name {
        "lhr_query" => {
            let request = bounded_query(parse(arguments)?, state)?;
            let dataset = VersionedDataset::open(&state.root).map_err(|error| error.to_string())?;
            let indexes = planner_indexes_for_request(&dataset, &request)
                .map_err(|error| error.to_string())?;
            let response = execute_query(&dataset, &request).map_err(|error| error.to_string())?;
            record_query(&state.root, &request, &response, indexes)
                .map_err(|error| error.to_string())?;
            serde_json::to_value(response).map_err(|error| error.to_string())
        }
        "lhr_row" => {
            let args: RowArgs = parse(arguments)?;
            let dataset = VersionedDataset::open(&state.root).map_err(|error| error.to_string())?;
            let values = dataset
                .row_values(args.row_id)
                .map_err(|error| error.to_string())?;
            Ok(match values {
                Some(values) => {
                    let mut row = Map::new();
                    for (column, value) in dataset.schema().columns.iter().zip(values.into_iter()) {
                        row.insert(
                            column.name.clone(),
                            value.map(Value::String).unwrap_or(Value::Null),
                        );
                    }
                    json!({"row_id":args.row_id,"found":true,"values":row})
                }
                None => json!({"row_id":args.row_id,"found":false}),
            })
        }
        "lhr_schema" => {
            let root = resolve_dataset_root(&state.root).map_err(|error| error.to_string())?;
            let schema = read_schema(root).map_err(|error| error.to_string())?;
            serde_json::to_value(schema).map_err(|error| error.to_string())
        }
        "lhr_stats" => serde_json::to_value(
            dataset_stats(&state.root).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string()),
        "lhr_explain" => {
            let args: ExplainArgs = parse(arguments)?;
            if args.predicates.is_empty() {
                return Err("at least one equality predicate is required".into());
            }
            let predicates: Vec<_> = args
                .predicates
                .into_iter()
                .map(|predicate| LogicalPredicate {
                    column: predicate.column,
                    value: predicate.value,
                })
                .collect();
            let dataset = VersionedDataset::open(&state.root).map_err(|error| error.to_string())?;
            let plan = dataset
                .explain_values(&predicates)
                .map_err(|error| error.to_string())?;
            serde_json::to_value(plan).map_err(|error| error.to_string())
        }
        "lhr_workload" => serde_json::to_value(
            workload_report(&state.root).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string()),
        "lhr_generations" => {
            let generations = list_generations(&state.root).map_err(|error| error.to_string())?;
            let leased = leased_generation_ids(&state.root).map_err(|error| error.to_string())?;
            Ok(json!({"generations":generations,"leased_generation_ids":leased}))
        }
        "lhr_indexes" => serde_json::to_value(
            list_indexes(&state.root).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string()),
        "lhr_diagnostics" => diagnostics(state),
        "lhr_benchmark_query" => benchmark_query(state, arguments),
        "lhr_verify" => serde_json::to_value(
            verify_versioned_dataset(&state.root).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string()),
        "lhr_mutate" => {
            let args: MutationArgs = parse(arguments)?;
            if args.mutations.is_empty() || args.mutations.len() > state.config.max_mutation_ops {
                return Err(format!(
                    "mutation operation count must be 1..={}",
                    state.config.max_mutation_ops
                ));
            }
            let config = mutation_config(&args.options, state)?;
            let detail = json!({"operations":args.mutations.len()});
            mutating(state, actor, "mutate", detail, || {
                apply_mutations_delta(&state.root, &args.mutations, &config)
            })
        }
        "lhr_compact" => {
            let args: CompactArgs = parse(arguments)?;
            let config = compaction_config(&args.options, state)?;
            mutating(state, actor, "compact", json!({}), || {
                compact_dataset(&state.root, &config)
            })
        }
        "lhr_vacuum" => {
            let args: VacuumArgs = parse(arguments)?;
            if args.retain == 0 {
                return Err("retain must be at least 1".into());
            }
            let detail = json!({"retain":args.retain,"protect":args.protect.clone()});
            mutating(state, actor, "vacuum", detail, || {
                vacuum_with_reader_leases(&state.root, args.retain, &args.protect)
            })
        }
        "lhr_recover" => mutating(state, actor, "recover", json!({}), || {
            recover_catalog(&state.root)
        }),
        "lhr_index_add" | "lhr_index_drop" | "lhr_index_rebuild" => {
            let args: IndexArgs = parse(arguments)?;
            if args.columns.len() < 2 {
                return Err("accelerator indexes require at least two columns".into());
            }
            let max_sort_records = args.max_sort_records.unwrap_or(250_000);
            if max_sort_records == 0 || max_sort_records > state.config.max_sort_records {
                return Err(format!(
                    "max_sort_records must be 1..={}",
                    state.config.max_sort_records
                ));
            }
            let detail = json!({
                "columns":args.columns.clone(),
                "max_sort_records":max_sort_records
            });
            match name {
                "lhr_index_add" => mutating(state, actor, "index_add", detail, || {
                    add_index(&state.root, &args.columns, max_sort_records)
                }),
                "lhr_index_drop" => mutating(state, actor, "index_drop", detail, || {
                    lhr::drop_index(&state.root, &args.columns)
                }),
                _ => mutating(state, actor, "index_rebuild", detail, || {
                    rebuild_index(&state.root, &args.columns, max_sort_records)
                }),
            }
        }
        _ => Err(format!("unknown MCP tool {name}")),
    }
}

fn diagnostics(state: &McpState) -> Result<Value, String> {
    let dataset = resolve_dataset_root(&state.root)
        .ok()
        .and_then(|root| dataset_status(root).ok())
        .and_then(|status| serde_json::to_value(status).ok());
    let leased = leased_generation_ids(&state.root).unwrap_or_default();
    Ok(json!({
        "process":process_metrics(),
        "filesystem":{
            "total_bytes":fs2::total_space(&state.root).ok(),
            "available_bytes":fs2::available_space(&state.root).ok()
        },
        "dataset":dataset,
        "leased_generation_ids":leased,
        "mcp":{
            "requests":state.counters.requests.load(Ordering::Relaxed),
            "failures":state.counters.failures.load(Ordering::Relaxed),
            "tool_calls":state.counters.tool_calls.load(Ordering::Relaxed),
            "auth_failures":state.counters.auth_failures.load(Ordering::Relaxed),
            "rate_limited":state.counters.rate_limited.load(Ordering::Relaxed),
            "max_concurrent_requests":state.config.max_concurrent_requests,
            "rate_limit_per_minute":state.config.rate_limit_per_minute,
            "max_query_limit":state.config.max_query_limit,
            "max_rows_examined":state.config.max_rows_examined,
            "max_query_timeout_ms":state.config.max_query_timeout_ms
        }
    }))
}

fn process_metrics() -> Value {
    let mut rss_bytes = None;
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(raw) = line.strip_prefix("VmRSS:") {
                rss_bytes = raw
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(|kib| kib.saturating_mul(1024));
            }
        }
    }
    let (minor_faults, major_faults) = process_faults();
    let mut read_bytes = None;
    let mut write_bytes = None;
    if let Ok(io_text) = fs::read_to_string("/proc/self/io") {
        for line in io_text.lines() {
            if let Some(value) = line
                .strip_prefix("read_bytes:")
                .and_then(|value| value.trim().parse::<u64>().ok())
            {
                read_bytes = Some(value);
            }
            if let Some(value) = line
                .strip_prefix("write_bytes:")
                .and_then(|value| value.trim().parse::<u64>().ok())
            {
                write_bytes = Some(value);
            }
        }
    }
    json!({
        "rss_bytes":rss_bytes,
        "minor_page_faults":minor_faults,
        "major_page_faults":major_faults,
        "read_bytes":read_bytes,
        "write_bytes":write_bytes
    })
}

fn process_faults() -> (u64, u64) {
    let Ok(stat) = fs::read_to_string("/proc/self/stat") else {
        return (0, 0);
    };
    let Some(end) = stat.rfind(')') else {
        return (0, 0);
    };
    // After the executable name, index 0 is field 3 (state). minflt is field 10 and majflt field 12.
    let fields: Vec<_> = stat[end + 1..].split_whitespace().collect();
    let minor = fields.get(7).and_then(|value| value.parse().ok()).unwrap_or(0);
    let major = fields.get(9).and_then(|value| value.parse().ok()).unwrap_or(0);
    (minor, major)
}

fn benchmark_query(state: &McpState, arguments: Value) -> Result<Value, String> {
    let args: BenchmarkArgs = parse(arguments)?;
    if args.iterations == 0 || args.iterations > 50 || args.warmup > 20 {
        return Err("benchmark iterations must be 1..=50 and warmup 0..=20".into());
    }
    let request = bounded_query(args.request, state)?;
    let dataset = VersionedDataset::open(&state.root).map_err(|error| error.to_string())?;
    for _ in 0..args.warmup {
        execute_query(&dataset, &request).map_err(|error| error.to_string())?;
    }
    let process_before = process_metrics();
    let faults_before = process_faults();
    let mut elapsed = Vec::with_capacity(args.iterations);
    let mut last = None;
    for _ in 0..args.iterations {
        let started = Instant::now();
        let response = execute_query(&dataset, &request).map_err(|error| error.to_string())?;
        elapsed.push(started.elapsed().as_micros());
        last = Some(response);
    }
    elapsed.sort_unstable();
    let faults_after = process_faults();
    Ok(json!({
        "iterations":args.iterations,
        "warmup":args.warmup,
        "latency_micros":{
            "min":elapsed.first().copied().unwrap_or(0),
            "median":percentile(&elapsed,0.50),
            "p95":percentile(&elapsed,0.95),
            "p99":percentile(&elapsed,0.99),
            "max":elapsed.last().copied().unwrap_or(0)
        },
        "process_before":process_before,
        "process_after":process_metrics(),
        "minor_faults_delta":faults_after.0.saturating_sub(faults_before.0),
        "major_faults_delta":faults_after.1.saturating_sub(faults_before.1),
        "last_query":last
    }))
}

fn percentile(values: &[u128], quantile: f64) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let index = ((values.len() - 1) as f64 * quantile).ceil() as usize;
    values[index.min(values.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_role_sees_mutation_but_not_admin_tools() {
        let names: Vec<_> = tool_catalog(ServiceRole::Write)
            .into_iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect();
        assert!(names.contains(&"lhr_mutate".to_string()));
        assert!(!names.contains(&"lhr_vacuum".to_string()));
    }

    #[test]
    fn percentile_is_bounded() {
        assert_eq!(percentile(&[1, 2, 3, 4], 0.50), 3);
        assert_eq!(percentile(&[1, 2, 3, 4], 0.99), 4);
    }
}

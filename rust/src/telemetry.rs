use crate::{list_indexes, LogicalPredicate, QueryFilter, QueryRequest, QueryResponse, VersionedDataset};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const TELEMETRY_DIR: &str = "telemetry";
const QUERY_LOG: &str = "queries.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryTelemetryEvent {
    pub timestamp_ms: u128,
    pub columns: Vec<String>,
    pub equality_columns: Vec<String>,
    pub operators: Vec<String>,
    pub elapsed_micros: u128,
    pub hits: u64,
    pub rows_examined: u64,
    pub pages_touched: u64,
    pub hierarchy_lookups: u64,
    pub optimized_equality_route: bool,
    #[serde(default)]
    pub used_indexes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryShapeStats {
    pub columns: Vec<String>,
    pub operators: Vec<String>,
    pub queries: u64,
    pub p50_micros: u128,
    pub p95_micros: u128,
    pub p99_micros: u128,
    pub avg_rows_examined: f64,
    pub max_rows_examined: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexUseStats {
    pub index: String,
    pub queries: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRecommendation {
    pub columns: Vec<String>,
    pub observed_queries: u64,
    pub avg_rows_examined: f64,
    pub score: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkloadReport {
    pub query_count: u64,
    pub p50_micros: u128,
    pub p95_micros: u128,
    pub p99_micros: u128,
    pub shapes: Vec<QueryShapeStats>,
    pub index_use: Vec<IndexUseStats>,
    pub recommendations: Vec<IndexRecommendation>,
}

fn telemetry_path(root: &Path) -> PathBuf {
    root.join(TELEMETRY_DIR).join(QUERY_LOG)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn filter_info(filters: &[QueryFilter]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut columns = Vec::new();
    let mut equality = Vec::new();
    let mut operators = Vec::new();
    for filter in filters {
        match filter {
            QueryFilter::Eq { column, .. } => {
                columns.push(column.clone());
                equality.push(column.clone());
                operators.push("eq".into());
            }
            QueryFilter::In { column, .. } => {
                columns.push(column.clone());
                operators.push("in".into());
            }
            QueryFilter::Range { column, .. } => {
                columns.push(column.clone());
                operators.push("range".into());
            }
        }
    }
    columns.sort();
    columns.dedup();
    equality.sort();
    equality.dedup();
    operators.sort();
    (columns, equality, operators)
}

pub fn planner_indexes_for_request(
    dataset: &VersionedDataset,
    request: &QueryRequest,
) -> io::Result<Vec<String>> {
    if request.filters.is_empty() {
        return Ok(Vec::new());
    }
    let mut predicates = Vec::with_capacity(request.filters.len());
    for filter in &request.filters {
        let QueryFilter::Eq { column, value } = filter else { return Ok(Vec::new()); };
        predicates.push(LogicalPredicate { column: column.clone(), value: value.clone() });
    }
    let mut used = BTreeSet::new();
    for (layer, explain) in dataset.explain_values(&predicates)? {
        if let Some(plan) = explain.plan {
            for index in plan.selected_indexes {
                used.insert(format!("layer:{layer}:{}:{}", index.kind, index.file));
            }
        }
    }
    Ok(used.into_iter().collect())
}

pub fn query_event(
    request: &QueryRequest,
    response: &QueryResponse,
    used_indexes: Vec<String>,
) -> QueryTelemetryEvent {
    let (columns, equality_columns, operators) = filter_info(&request.filters);
    QueryTelemetryEvent {
        timestamp_ms: now_ms(),
        columns,
        equality_columns,
        operators,
        elapsed_micros: response.stats.elapsed_micros,
        hits: response.stats.hits,
        rows_examined: response.stats.rows_examined,
        pages_touched: response.stats.pages_touched,
        hierarchy_lookups: response.stats.hierarchy_lookups,
        optimized_equality_route: response.stats.optimized_equality_route,
        used_indexes,
    }
}

pub fn append_query_event(root: impl AsRef<Path>, event: &QueryTelemetryEvent) -> io::Result<()> {
    let path = telemetry_path(root.as_ref());
    fs::create_dir_all(path.parent().unwrap())?;
    let mut file = OpenOptions::new().create(true).append(true).read(true).open(path)?;
    file.lock_exclusive()?;
    let result = (|| {
        serde_json::to_writer(&mut file, event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        file.write_all(b"\n")?;
        file.sync_data()
    })();
    let _ = FileExt::unlock(&file);
    result
}

pub fn record_query(
    root: impl AsRef<Path>,
    request: &QueryRequest,
    response: &QueryResponse,
    used_indexes: Vec<String>,
) -> io::Result<()> {
    append_query_event(root, &query_event(request, response, used_indexes))
}

pub fn load_query_events(root: impl AsRef<Path>) -> io::Result<Vec<QueryTelemetryEvent>> {
    let path = telemetry_path(root.as_ref());
    if !path.exists() { return Ok(Vec::new()); }
    let file = File::open(path)?;
    let mut events = Vec::new();
    for (line_no, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        let event = serde_json::from_str(&line).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("telemetry line {}: {e}", line_no + 1))
        })?;
        events.push(event);
    }
    Ok(events)
}

fn percentile(mut values: Vec<u128>, percentile: f64) -> u128 {
    if values.is_empty() { return 0; }
    values.sort_unstable();
    let index = ((values.len() - 1) as f64 * percentile).round() as usize;
    values[index]
}

pub fn workload_report(root: impl AsRef<Path>) -> io::Result<WorkloadReport> {
    let root = root.as_ref();
    let events = load_query_events(root)?;
    let all_latency: Vec<_> = events.iter().map(|x| x.elapsed_micros).collect();

    let mut shapes = BTreeMap::<(Vec<String>, Vec<String>), Vec<&QueryTelemetryEvent>>::new();
    let mut index_use = BTreeMap::<String, u64>::new();
    for event in &events {
        shapes.entry((event.columns.clone(), event.operators.clone())).or_default().push(event);
        for index in &event.used_indexes { *index_use.entry(index.clone()).or_default() += 1; }
    }

    let mut shape_stats = Vec::new();
    for ((columns, operators), group) in shapes {
        let latency: Vec<_> = group.iter().map(|x| x.elapsed_micros).collect();
        let total_rows: u128 = group.iter().map(|x| x.rows_examined as u128).sum();
        let max_rows = group.iter().map(|x| x.rows_examined).max().unwrap_or(0);
        shape_stats.push(QueryShapeStats {
            columns, operators, queries: group.len() as u64,
            p50_micros: percentile(latency.clone(), 0.50),
            p95_micros: percentile(latency.clone(), 0.95),
            p99_micros: percentile(latency, 0.99),
            avg_rows_examined: total_rows as f64 / group.len() as f64,
            max_rows_examined: max_rows,
        });
    }
    shape_stats.sort_by(|a, b| b.queries.cmp(&a.queries).then_with(|| b.p95_micros.cmp(&a.p95_micros)));

    let existing: BTreeSet<Vec<String>> = list_indexes(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|x| x.exact_rows && x.column_names.len() >= 2)
        .map(|mut x| { x.column_names.sort(); x.column_names })
        .collect();
    let mut recommendation_groups = BTreeMap::<Vec<String>, (u64, u128)>::new();
    for event in &events {
        if event.equality_columns.len() < 2 || existing.contains(&event.equality_columns) { continue; }
        let entry = recommendation_groups.entry(event.equality_columns.clone()).or_default();
        entry.0 += 1;
        entry.1 += event.rows_examined as u128;
    }
    let mut recommendations: Vec<_> = recommendation_groups
        .into_iter()
        .map(|(columns, (queries, rows))| {
            let avg = rows as f64 / queries as f64;
            IndexRecommendation {
                columns, observed_queries: queries, avg_rows_examined: avg,
                score: queries as f64 * (avg + 1.0).ln_1p(),
                reason: "frequent multi-column equality shape without an exact accelerator".into(),
            }
        })
        .collect();
    recommendations.sort_by(|a, b| b.score.total_cmp(&a.score));

    Ok(WorkloadReport {
        query_count: events.len() as u64,
        p50_micros: percentile(all_latency.clone(), 0.50),
        p95_micros: percentile(all_latency.clone(), 0.95),
        p99_micros: percentile(all_latency, 0.99),
        shapes: shape_stats,
        index_use: index_use.into_iter().map(|(index, queries)| IndexUseStats { index, queries }).collect(),
        recommendations,
    })
}

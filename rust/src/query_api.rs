use crate::{DatasetSchema, LogicalPredicate, LogicalType, NamedValue, VersionedDataset};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, io, time::{Duration, Instant}};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum QueryFilter {
    Eq { column: String, value: Option<String> },
    In { column: String, values: Vec<Option<String>> },
    Range {
        column: String,
        #[serde(default)]
        gte: Option<String>,
        #[serde(default)]
        lte: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub filters: Vec<QueryFilter>,
    #[serde(default)]
    pub select: Vec<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub after_row_id: Option<u64>,
    #[serde(default)]
    pub max_rows_examined: Option<u64>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

fn default_limit() -> usize { 100 }

#[derive(Debug, Clone, Serialize)]
pub struct QueryApiRow {
    pub row_id: u64,
    pub values: Vec<NamedValue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryApiStats {
    pub hits: u64,
    pub rows_examined: u64,
    pub pages_touched: u64,
    pub hierarchy_lookups: u64,
    pub elapsed_micros: u128,
    pub optimized_equality_route: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResponse {
    pub rows: Vec<QueryApiRow>,
    pub returned: usize,
    pub next_cursor: Option<u64>,
    pub stats: QueryApiStats,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn deadline(request: &QueryRequest, start: Instant) -> Option<Instant> {
    request.timeout_ms.and_then(|ms| start.checked_add(Duration::from_millis(ms)))
}

fn enforce_deadline(deadline: Option<Instant>) -> io::Result<()> {
    if deadline.is_some_and(|x| Instant::now() >= x) {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "query timeout exceeded"));
    }
    Ok(())
}

fn canonical_optional(
    schema: &DatasetSchema,
    column: usize,
    value: &Option<String>,
) -> io::Result<Option<String>> {
    let column_schema = &schema.columns[column];
    match value {
        None => {
            if !column_schema.nullable {
                return Err(invalid(format!("column {} is not nullable", column_schema.name)));
            }
            Ok(None)
        }
        Some(raw) if column_schema.is_null_literal(raw) => Ok(None),
        Some(raw) => Ok(Some(column_schema.canonicalize(raw)?)),
    }
}

fn compare_typed(kind: &LogicalType, left: &str, right: &str) -> io::Result<Ordering> {
    match kind {
        LogicalType::Unsigned => Ok(left
            .parse::<u64>()
            .map_err(|e| invalid(format!("invalid canonical unsigned value: {e}")))?
            .cmp(&right.parse::<u64>().map_err(|e| invalid(format!("invalid range unsigned value: {e}")))?)),
        LogicalType::Signed => Ok(left
            .parse::<i64>()
            .map_err(|e| invalid(format!("invalid canonical signed value: {e}")))?
            .cmp(&right.parse::<i64>().map_err(|e| invalid(format!("invalid range signed value: {e}")))?)),
        other => Err(invalid(format!(
            "range predicates currently require signed/unsigned columns, got {other:?}"
        ))),
    }
}

#[derive(Clone)]
enum PreparedFilter {
    Eq { column: usize, value: Option<String> },
    In { column: usize, values: Vec<Option<String>> },
    Range { column: usize, gte: Option<String>, lte: Option<String> },
}

fn prepare_filters(schema: &DatasetSchema, filters: &[QueryFilter]) -> io::Result<Vec<PreparedFilter>> {
    if filters.is_empty() { return Err(invalid("at least one query filter is required")); }
    let mut out = Vec::with_capacity(filters.len());
    for filter in filters {
        match filter {
            QueryFilter::Eq { column, value } => {
                let index = schema.column_index(column).ok_or_else(|| invalid(format!("unknown column {column}")))?;
                out.push(PreparedFilter::Eq { column: index, value: canonical_optional(schema, index, value)? });
            }
            QueryFilter::In { column, values } => {
                if values.is_empty() { return Err(invalid(format!("IN filter for {column} is empty"))); }
                let index = schema.column_index(column).ok_or_else(|| invalid(format!("unknown column {column}")))?;
                let mut canonical = Vec::with_capacity(values.len());
                for value in values { canonical.push(canonical_optional(schema, index, value)?); }
                canonical.sort();
                canonical.dedup();
                out.push(PreparedFilter::In { column: index, values: canonical });
            }
            QueryFilter::Range { column, gte, lte } => {
                if gte.is_none() && lte.is_none() { return Err(invalid(format!("range for {column} has no bound"))); }
                let index = schema.column_index(column).ok_or_else(|| invalid(format!("unknown column {column}")))?;
                if !matches!(schema.columns[index].logical_type, LogicalType::Unsigned | LogicalType::Signed) {
                    return Err(invalid(format!("range predicates require signed/unsigned column {column}")));
                }
                let gte = gte.as_ref().map(|x| schema.columns[index].canonicalize(x)).transpose()?;
                let lte = lte.as_ref().map(|x| schema.columns[index].canonicalize(x)).transpose()?;
                if let (Some(lo), Some(hi)) = (&gte, &lte) {
                    if compare_typed(&schema.columns[index].logical_type, lo, hi)? == Ordering::Greater {
                        return Err(invalid(format!("range lower bound exceeds upper bound for {column}")));
                    }
                }
                out.push(PreparedFilter::Range { column: index, gte, lte });
            }
        }
    }
    Ok(out)
}

fn matches_filters(schema: &DatasetSchema, values: &[Option<String>], filters: &[PreparedFilter]) -> io::Result<bool> {
    for filter in filters {
        match filter {
            PreparedFilter::Eq { column, value } => {
                if values.get(*column) != Some(value) { return Ok(false); }
            }
            PreparedFilter::In { column, values: accepted } => {
                let Some(value) = values.get(*column) else { return Ok(false); };
                if accepted.binary_search(value).is_err() { return Ok(false); }
            }
            PreparedFilter::Range { column, gte, lte } => {
                let Some(Some(value)) = values.get(*column) else { return Ok(false); };
                let kind = &schema.columns[*column].logical_type;
                if let Some(lo) = gte {
                    if compare_typed(kind, value, lo)? == Ordering::Less { return Ok(false); }
                }
                if let Some(hi) = lte {
                    if compare_typed(kind, value, hi)? == Ordering::Greater { return Ok(false); }
                }
            }
        }
    }
    Ok(true)
}

fn projection(schema: &DatasetSchema, select: &[String]) -> io::Result<Vec<usize>> {
    if select.is_empty() { return Ok((0..schema.columns.len()).collect()); }
    select.iter().map(|name| schema.column_index(name).ok_or_else(|| invalid(format!("unknown selected column {name}")))).collect()
}

fn equality_predicates(filters: &[PreparedFilter], schema: &DatasetSchema) -> Option<Vec<LogicalPredicate>> {
    let mut out = Vec::with_capacity(filters.len());
    for filter in filters {
        let PreparedFilter::Eq { column, value } = filter else { return None; };
        out.push(LogicalPredicate { column: schema.columns[*column].name.clone(), value: value.clone() });
    }
    Some(out)
}

pub fn execute_query(dataset: &VersionedDataset, request: &QueryRequest) -> io::Result<QueryResponse> {
    if request.limit == 0 { return Err(invalid("query limit must be > 0")); }
    let start = Instant::now();
    let deadline = deadline(request, start);
    let prepared = prepare_filters(dataset.schema(), &request.filters)?;
    let projection = projection(dataset.schema(), &request.select)?;

    if let Some(eq) = equality_predicates(&prepared, dataset.schema()) {
        // Equality predicates retain the optimized exact-index route. The stable logical-row cursor
        // is translated to a lower bound inside each layer instead of replaying a growing prefix.
        enforce_deadline(deadline)?;
        let result = dataset.query_values_after(
            &eq,
            if request.select.is_empty() { None } else { Some(&request.select) },
            request.after_row_id,
            request.limit,
        )?;
        if request.max_rows_examined.is_some_and(|max| result.rows_checked > max) {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "query row-examination limit exceeded"));
        }
        enforce_deadline(deadline)?;
        let mut rows: Vec<_> = result.rows.into_iter()
            .map(|row| QueryApiRow { row_id: row.row_id, values: row.values })
            .collect();
        let next_cursor = (rows.len() == request.limit).then(|| rows.last().unwrap().row_id);
        return Ok(QueryResponse {
            returned: rows.len(),
            rows: std::mem::take(&mut rows),
            next_cursor,
            stats: QueryApiStats {
                hits: result.hits,
                rows_examined: result.rows_checked,
                pages_touched: result.pages_touched,
                hierarchy_lookups: result.hierarchy_lookups,
                elapsed_micros: start.elapsed().as_micros(),
                optimized_equality_route: true,
            },
        });
    }

    // Set/range filters use the exact canonical/versioned fallback. It is deliberately slower but
    // never probabilistic and is bounded by explicit resource controls.
    let mut rows_examined = 0u64;
    let mut hits = 0u64;
    let mut rows = Vec::new();
    dataset.for_each_visible_row(|row_id, values| {
        rows_examined = rows_examined.saturating_add(1);
        if request.max_rows_examined.is_some_and(|max| rows_examined > max) {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "query row-examination limit exceeded"));
        }
        if rows_examined % 1024 == 0 { enforce_deadline(deadline)?; }
        if !matches_filters(dataset.schema(), &values, &prepared)? { return Ok(()); }
        hits = hits.saturating_add(1);
        if request.after_row_id.is_some_and(|cursor| row_id <= cursor) || rows.len() >= request.limit { return Ok(()); }
        let selected = projection.iter().map(|&column| NamedValue {
            column: dataset.schema().columns[column].name.clone(),
            value: values[column].clone(),
        }).collect();
        rows.push(QueryApiRow { row_id, values: selected });
        Ok(())
    })?;
    enforce_deadline(deadline)?;
    let next_cursor = (rows.len() == request.limit).then(|| rows.last().unwrap().row_id);
    Ok(QueryResponse {
        returned: rows.len(), rows, next_cursor,
        stats: QueryApiStats {
            hits, rows_examined, pages_touched: 0, hierarchy_lookups: 0,
            elapsed_micros: start.elapsed().as_micros(), optimized_equality_route: false,
        },
    })
}

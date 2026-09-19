use crate::{DatasetSchema, LogicalPredicate, LogicalType, NamedValue, VersionedDataset};
use serde::{Deserialize, Serialize};
use std::{
    cmp::{Ordering, Reverse},
    collections::BinaryHeap,
    io,
    time::{Duration, Instant},
};

const MAX_EXACT_RANGE_VALUES: u64 = 256;

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

fn enforce_rows_examined(max: Option<u64>, rows_examined: u64) -> io::Result<()> {
    if max.is_some_and(|limit| rows_examined > limit) {
        return Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            "query row-examination limit exceeded",
        ));
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

fn equality_subset(filters: &[PreparedFilter], schema: &DatasetSchema) -> (Vec<usize>, Vec<LogicalPredicate>) {
    let mut columns = Vec::new();
    let mut predicates = Vec::new();
    for filter in filters {
        if let PreparedFilter::Eq { column, value } = filter {
            columns.push(*column);
            predicates.push(LogicalPredicate {
                column: schema.columns[*column].name.clone(),
                value: value.clone(),
            });
        }
    }
    (columns, predicates)
}

fn equality_predicates(filters: &[PreparedFilter], schema: &DatasetSchema) -> Option<Vec<LogicalPredicate>> {
    let mut out = Vec::with_capacity(filters.len());
    for filter in filters {
        let PreparedFilter::Eq { column, value } = filter else { return None; };
        out.push(LogicalPredicate { column: schema.columns[*column].name.clone(), value: value.clone() });
    }
    Some(out)
}

fn bounded_integer_range_values(
    filters: &[PreparedFilter],
    schema: &DatasetSchema,
) -> Option<(usize, Vec<String>)> {
    if filters.len() != 1 {
        return None;
    }
    let PreparedFilter::Range { column, gte, lte } = &filters[0] else {
        return None;
    };
    let (Some(gte), Some(lte)) = (gte.as_deref(), lte.as_deref()) else {
        return None;
    };

    match schema.columns[*column].logical_type {
        LogicalType::Unsigned => {
            let lo = gte.parse::<u64>().ok()?;
            let hi = lte.parse::<u64>().ok()?;
            let width = hi.checked_sub(lo)?.checked_add(1)?;
            if width > MAX_EXACT_RANGE_VALUES {
                return None;
            }
            let values = (0..width)
                .map(|offset| lo.checked_add(offset).map(|value| value.to_string()))
                .collect::<Option<Vec<_>>>()?;
            Some((*column, values))
        }
        LogicalType::Signed => {
            let lo = gte.parse::<i64>().ok()? as i128;
            let hi = lte.parse::<i64>().ok()? as i128;
            let width = hi.checked_sub(lo)?.checked_add(1)?;
            if width <= 0 || width > MAX_EXACT_RANGE_VALUES as i128 {
                return None;
            }
            let values = (0..width as u64)
                .map(|offset| lo.checked_add(offset as i128).map(|value| value.to_string()))
                .collect::<Option<Vec<_>>>()?;
            Some((*column, values))
        }
        _ => None,
    }
}

pub fn execute_query(dataset: &VersionedDataset, request: &QueryRequest) -> io::Result<QueryResponse> {
    if request.limit == 0 { return Err(invalid("query limit must be > 0")); }
    let start = Instant::now();
    let deadline = deadline(request, start);
    let prepared = prepare_filters(dataset.schema(), &request.filters)?;
    let projection = projection(dataset.schema(), &request.select)?;

    // A zero-filter request is the exact table-browse path used by Studio and API clients.
    // It walks stable logical IDs forward from the cursor and materializes only the requested
    // page, so deep pagination does not replay every prior row.
    if prepared.is_empty() {
        let mut rows_examined = 0u64;
        let mut rows = Vec::with_capacity(request.limit);
        let mut row_id = request
            .after_row_id
            .map(|value| value.saturating_add(1))
            .unwrap_or(0);
        if let Some(max_row_id) = dataset.max_row_id() {
            while row_id <= max_row_id && rows.len() < request.limit {
                rows_examined = rows_examined.saturating_add(1);
                enforce_rows_examined(request.max_rows_examined, rows_examined)?;
                if rows_examined % 1024 == 0 {
                    enforce_deadline(deadline)?;
                }
                if let Some(values) = dataset.row_values(row_id)? {
                    let selected = projection
                        .iter()
                        .map(|&column| NamedValue {
                            column: dataset.schema().columns[column].name.clone(),
                            value: values[column].clone(),
                        })
                        .collect();
                    rows.push(QueryApiRow { row_id, values: selected });
                }
                if row_id == u64::MAX {
                    break;
                }
                row_id += 1;
            }
        }
        enforce_deadline(deadline)?;
        let next_cursor = (rows.len() == request.limit)
            .then(|| rows.last().unwrap().row_id)
            .filter(|cursor| dataset.max_row_id().is_some_and(|max| *cursor < max));
        return Ok(QueryResponse {
            returned: rows.len(),
            rows,
            next_cursor,
            stats: QueryApiStats {
                hits: dataset.visible_rows(),
                rows_examined,
                pages_touched: 0,
                hierarchy_lookups: 0,
                elapsed_micros: start.elapsed().as_micros(),
                optimized_equality_route: false,
            },
        });
    }

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
        enforce_rows_examined(request.max_rows_examined, result.rows_checked)?;
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


    // When exact equality filters coexist with filters that do not have a direct access path,
    // drive the query from the equality intersection first and evaluate the residual predicates
    // only against that bounded stream. This avoids scanning the whole visible dataset while also
    // avoiding materializing the entire candidate set in memory.
    let (candidate_columns, candidate_predicates) = equality_subset(&prepared, dataset.schema());
    if !candidate_predicates.is_empty()
        && candidate_predicates.len() < prepared.len()
        && candidate_columns
            .iter()
            .all(|column| dataset.has_exact_singleton(*column))
    {
        const CANDIDATE_BATCH: usize = 4096;
        let mut rows_examined = 0u64;
        let mut hits = 0u64;
        let mut pages_touched = 0u64;
        let mut hierarchy_lookups = 0u64;
        let mut rows = Vec::with_capacity(request.limit);
        let mut candidate_after = None;

        loop {
            enforce_deadline(deadline)?;
            let result = dataset.query_values_after(
                &candidate_predicates,
                None,
                candidate_after,
                CANDIDATE_BATCH,
            )?;
            pages_touched = pages_touched.saturating_add(result.pages_touched);
            hierarchy_lookups = hierarchy_lookups.saturating_add(result.hierarchy_lookups);
            enforce_rows_examined(
                request.max_rows_examined,
                rows_examined.saturating_add(result.rows_checked),
            )?;

            let returned = result.rows.len();
            if returned == 0 {
                break;
            }
            let mut last_row_id = candidate_after;
            for candidate in result.rows {
                rows_examined = rows_examined.saturating_add(1);
                enforce_rows_examined(request.max_rows_examined, rows_examined)?;
                if rows_examined % 1024 == 0 {
                    enforce_deadline(deadline)?;
                }

                last_row_id = Some(candidate.row_id);
                let values = candidate
                    .values
                    .into_iter()
                    .map(|value| value.value)
                    .collect::<Vec<_>>();
                if !matches_filters(dataset.schema(), &values, &prepared)? {
                    continue;
                }
                hits = hits.saturating_add(1);
                if request.after_row_id.is_some_and(|cursor| candidate.row_id <= cursor)
                    || rows.len() >= request.limit
                {
                    continue;
                }
                let selected = projection
                    .iter()
                    .map(|&column| NamedValue {
                        column: dataset.schema().columns[column].name.clone(),
                        value: values[column].clone(),
                    })
                    .collect();
                rows.push(QueryApiRow {
                    row_id: candidate.row_id,
                    values: selected,
                });
            }

            if returned < CANDIDATE_BATCH {
                break;
            }
            let Some(next_after) = last_row_id else {
                break;
            };
            if candidate_after.is_some_and(|old| next_after <= old) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "candidate-first planner cursor did not advance",
                ));
            }
            candidate_after = Some(next_after);
        }

        enforce_deadline(deadline)?;
        let next_cursor = (rows.len() == request.limit)
            .then(|| rows.last().unwrap().row_id);
        return Ok(QueryResponse {
            returned: rows.len(),
            rows,
            next_cursor,
            stats: QueryApiStats {
                hits,
                rows_examined,
                pages_touched,
                hierarchy_lookups,
                elapsed_micros: start.elapsed().as_micros(),
                optimized_equality_route: true,
            },
        });
    }

    // Hypothesis B0: a narrow bounded integer range can be answered exactly with the existing
    // singleton equality indexes. Each integer value becomes one exact equality stream; the
    // streams are merged by stable logical row ID while retaining at most one head row per value.
    // This deliberately spends extra hierarchy lookups to avoid adding an on-disk range index.
    if let Some((column, range_values)) = bounded_integer_range_values(&prepared, dataset.schema()) {
        if dataset.has_exact_singleton(column) {
            let column_name = dataset.schema().columns[column].name.clone();
            let select = if request.select.is_empty() {
                None
            } else {
                Some(request.select.as_slice())
            };
            let mut heads: Vec<Option<QueryApiRow>> =
                (0..range_values.len()).map(|_| None).collect();
            let mut heap = BinaryHeap::<Reverse<(u64, usize)>>::new();
            let mut hits = 0u64;
            let mut rows_examined = 0u64;
            let mut pages_touched = 0u64;
            let mut hierarchy_lookups = 0u64;

            for (stream, value) in range_values.iter().enumerate() {
                enforce_deadline(deadline)?;
                let predicate = [LogicalPredicate {
                    column: column_name.clone(),
                    value: Some(value.clone()),
                }];
                let result =
                    dataset.query_values_after(&predicate, select, request.after_row_id, 1)?;
                hits = hits.saturating_add(result.hits);
                rows_examined = rows_examined.saturating_add(result.rows_checked);
                pages_touched = pages_touched.saturating_add(result.pages_touched);
                hierarchy_lookups = hierarchy_lookups.saturating_add(result.hierarchy_lookups);
                enforce_rows_examined(request.max_rows_examined, rows_examined)?;
                if let Some(row) = result.rows.into_iter().next() {
                    let row = QueryApiRow { row_id: row.row_id, values: row.values };
                    heap.push(Reverse((row.row_id, stream)));
                    heads[stream] = Some(row);
                }
            }

            let mut rows = Vec::with_capacity(request.limit);
            while rows.len() < request.limit {
                let Some(Reverse((row_id, stream))) = heap.pop() else {
                    break;
                };
                let Some(row) = heads[stream].take() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "range stream heap/head state diverged",
                    ));
                };
                rows.push(row);
                if rows.len() >= request.limit {
                    break;
                }

                enforce_deadline(deadline)?;
                let predicate = [LogicalPredicate {
                    column: column_name.clone(),
                    value: Some(range_values[stream].clone()),
                }];
                let result = dataset.query_values_after(&predicate, select, Some(row_id), 1)?;
                rows_examined = rows_examined.saturating_add(result.rows_checked);
                pages_touched = pages_touched.saturating_add(result.pages_touched);
                hierarchy_lookups = hierarchy_lookups.saturating_add(result.hierarchy_lookups);
                enforce_rows_examined(request.max_rows_examined, rows_examined)?;
                if let Some(next) = result.rows.into_iter().next() {
                    let next = QueryApiRow { row_id: next.row_id, values: next.values };
                    heap.push(Reverse((next.row_id, stream)));
                    heads[stream] = Some(next);
                }
            }

            enforce_deadline(deadline)?;
            let next_cursor = (rows.len() == request.limit).then(|| rows.last().unwrap().row_id);
            return Ok(QueryResponse {
                returned: rows.len(),
                rows,
                next_cursor,
                stats: QueryApiStats {
                    hits,
                    rows_examined,
                    pages_touched,
                    hierarchy_lookups,
                    elapsed_micros: start.elapsed().as_micros(),
                    optimized_equality_route: false,
                },
            });
        }
    }

    // Set/range filters use the exact canonical/versioned fallback. It is deliberately slower but
    // never probabilistic and is bounded by explicit resource controls.
    let mut rows_examined = 0u64;
    let mut hits = 0u64;
    let mut rows = Vec::new();
    dataset.for_each_visible_row(|row_id, values| {
        rows_examined = rows_examined.saturating_add(1);
        enforce_rows_examined(request.max_rows_examined, rows_examined)?;
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

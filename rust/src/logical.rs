use crate::{
    numeric_order_filename, read_schema, resolve_dataset_root, DatasetSchema, DecodedValue,
    Dictionary, Engine, Manifest, NumericOrder, Predicate, QueryExplain, QueryStats, RowIdMap,
};
use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NamedValue {
    pub column: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LogicalPredicate {
    pub column: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LogicalRow {
    pub row_id: u64,
    pub values: Vec<NamedValue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogicalQueryResult {
    pub hits: u64,
    pub returned: usize,
    pub rows_checked: u64,
    pub pages_touched: u64,
    pub hierarchy_lookups: u64,
    pub rows: Vec<LogicalRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogicalExplain {
    pub predicates: Vec<LogicalPredicate>,
    pub dictionary_miss: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<QueryExplain>,
}

pub fn dictionary_filename(column: usize) -> String {
    format!("c{column:04}.dict")
}

pub struct LogicalDataset {
    root: PathBuf,
    schema: DatasetSchema,
    dictionaries: Vec<Dictionary>,
    numeric_orders: Vec<Option<NumericOrder>>,
    engine: Engine,
    row_ids: RowIdMap,
    rows: u64,
    exact_singletons: Vec<bool>,
}

impl LogicalDataset {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = resolve_dataset_root(root)?;
        let schema = read_schema(&root)?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if schema.columns.len() != manifest.columns {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "schema column count does not match manifest",
            ));
        }
        let mut exact_singletons = vec![false; manifest.columns];
        for hierarchy in &manifest.hierarchies {
            if hierarchy.columns.len() == 1
                && matches!(
                    hierarchy.kind.as_str(),
                    "postings" | "densepost" | "deltapost" | "flatpost" | "bitslice"
                )
            {
                let column = hierarchy.columns[0];
                if column < exact_singletons.len() {
                    exact_singletons[column] = true;
                }
            }
        }
        let mut dictionaries = Vec::with_capacity(schema.columns.len());
        for (index, column) in schema.columns.iter().enumerate() {
            let dict = Dictionary::open(root.join("dictionaries").join(dictionary_filename(index)))?;
            if dict.nullable() != column.nullable {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("dictionary nullability mismatch for {}", column.name),
                ));
            }
            if dict.cardinality() != manifest.cardinalities[index] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("dictionary cardinality mismatch for {}", column.name),
                ));
            }
            dictionaries.push(dict);
        }
        let mut numeric_orders = Vec::with_capacity(schema.columns.len());
        for (index, column) in schema.columns.iter().enumerate() {
            let path = root.join("routing").join(numeric_order_filename(index));
            if path.is_file() {
                let order = NumericOrder::open(&path)?;
                order.validate_for(&dictionaries[index], &column.logical_type)?;
                numeric_orders.push(Some(order));
            } else {
                numeric_orders.push(None);
            }
        }

        let engine = Engine::open(&root)?;
        let row_ids = RowIdMap::open_optional(&root, manifest.rows)?;
        Ok(Self {
            root,
            schema,
            dictionaries,
            numeric_orders,
            engine,
            row_ids,
            rows: manifest.rows,
            exact_singletons,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn schema(&self) -> &DatasetSchema {
        &self.schema
    }

    pub fn physical_rows(&self) -> u64 {
        self.rows
    }

    pub fn has_exact_singleton(&self, column: usize) -> bool {
        self.exact_singletons.get(column).copied().unwrap_or(false)
    }

    pub fn logical_row_id(&self, physical: u64) -> Option<u64> {
        self.row_ids.logical(physical)
    }

    pub fn physical_row_id(&self, logical: u64) -> Option<u64> {
        self.row_ids.physical(logical)
    }

    pub fn max_row_id(&self) -> Option<u64> {
        self.row_ids.max_id()
    }

    pub fn contains_canonical_value(&self, column: usize, value: &str) -> bool {
        self.dictionaries
            .get(column)
            .and_then(|dictionary| dictionary.token(value))
            .is_some()
    }

    pub fn decode_physical_values(&self, physical: u64) -> io::Result<Vec<Option<String>>> {
        let tokens = self.engine.row(physical).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "physical row ID does not exist")
        })?;
        let mut values = Vec::with_capacity(tokens.len());
        for (column, raw_token) in tokens.into_iter().enumerate() {
            let token = u32::try_from(raw_token).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "row token exceeds u32")
            })?;
            let decoded = self.dictionaries[column].decode(token).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "row token absent from dictionary")
            })?;
            values.push(match decoded {
                DecodedValue::Null => None,
                DecodedValue::Text(text) => Some(text.to_owned()),
            });
        }
        Ok(values)
    }

    fn decode_physical_projection(
        &self,
        physical: u64,
        projection: &[usize],
    ) -> io::Result<Vec<Option<String>>> {
        let tokens = self.engine.row_projection(physical, projection).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "physical row ID does not exist")
        })?;
        let mut values = Vec::with_capacity(tokens.len());
        for (&column, raw_token) in projection.iter().zip(tokens) {
            let token = u32::try_from(raw_token).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "row token exceeds u32")
            })?;
            let decoded = self.dictionaries[column].decode(token).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "row token absent from dictionary")
            })?;
            values.push(match decoded {
                DecodedValue::Null => None,
                DecodedValue::Text(text) => Some(text.to_owned()),
            });
        }
        Ok(values)
    }

    fn encoded_predicates(
        &self,
        predicates: &[LogicalPredicate],
    ) -> io::Result<Option<Vec<Predicate>>> {
        let mut out = Vec::with_capacity(predicates.len());
        for predicate in predicates {
            let index = self.schema.column_index(&predicate.column).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown column {}", predicate.column),
                )
            })?;
            let column = &self.schema.columns[index];
            let token = match predicate.value.as_deref() {
                None => {
                    if !column.nullable {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("column {} is not nullable", column.name),
                        ));
                    }
                    0
                }
                Some(raw) if column.is_null_literal(raw) => 0,
                Some(raw) => {
                    let canonical = column.canonicalize(raw)?;
                    let Some(token) = self.dictionaries[index].token(&canonical) else {
                        return Ok(None);
                    };
                    token
                }
            };
            out.push(Predicate {
                column: index,
                value: token as u64,
            });
        }
        Ok(Some(out))
    }

    fn projection(&self, select: Option<&[String]>) -> io::Result<Vec<usize>> {
        match select {
            None => Ok((0..self.schema.columns.len()).collect()),
            Some(names) => names
                .iter()
                .map(|name| {
                    self.schema.column_index(name).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown selected column {name}"),
                        )
                    })
                })
                .collect(),
        }
    }

    fn first_physical_after(&self, after_row_id: Option<u64>) -> u64 {
        let Some(cursor) = after_row_id else {
            return 0;
        };
        let mut lo = 0u64;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.row_ids.logical(mid) {
                Some(row_id) if row_id <= cursor => lo = mid + 1,
                Some(_) => hi = mid,
                None => hi = mid,
            }
        }
        lo
    }

    pub fn explain_values(&self, predicates: &[LogicalPredicate]) -> io::Result<LogicalExplain> {
        let encoded = self.encoded_predicates(predicates)?;
        Ok(match encoded {
            None => LogicalExplain {
                predicates: predicates.to_vec(),
                dictionary_miss: true,
                plan: None,
            },
            Some(encoded) => LogicalExplain {
                predicates: predicates.to_vec(),
                dictionary_miss: false,
                plan: Some(self.engine.explain(&encoded)),
            },
        })
    }

    pub fn query_values(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        self.query_values_after(predicates, select, None, limit)
    }

    pub fn query_values_after(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        after_row_id: Option<u64>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        let projection = self.projection(select)?;
        let Some(encoded) = self.encoded_predicates(predicates)? else {
            return Ok(LogicalQueryResult {
                hits: 0,
                returned: 0,
                rows_checked: 0,
                pages_touched: 0,
                hierarchy_lookups: 0,
                rows: Vec::new(),
            });
        };
        let first_physical = self.first_physical_after(after_row_id);
        let (physical_ids, stats) =
            self.engine.query_row_ids_from(&encoded, limit, first_physical);
        let mut rows = Vec::with_capacity(physical_ids.len());
        for physical in physical_ids {
            // Preserve the established normal exact-query materialization path. Projection-aware
            // reads are reserved for residual-filter streaming below.
            let decoded = self.decode_physical_values(physical)?;
            let mut values = Vec::with_capacity(projection.len());
            for &column in &projection {
                values.push(NamedValue {
                    column: self.schema.columns[column].name.clone(),
                    value: decoded[column].clone(),
                });
            }
            let row_id = self.row_ids.logical(physical).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "query returned unmapped row ID")
            })?;
            rows.push(LogicalRow { row_id, values });
        }
        Ok(LogicalQueryResult {
            hits: stats.hits,
            returned: rows.len(),
            rows_checked: stats.rows_checked,
            pages_touched: stats.pages_touched,
            hierarchy_lookups: stats.hierarchy_lookups,
            rows,
        })
    }

    /// Internal page-only exact route. Unlike query_values_after, this does not compute a
    /// global hit count; it is used by higher-level residual-filter execution to consume the
    /// equality candidate stream without rebuilding the complete conjunction for each page.
    pub(crate) fn query_values_page_after(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        after_row_id: Option<u64>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        let projection = self.projection(select)?;
        let Some(encoded) = self.encoded_predicates(predicates)? else {
            return Ok(LogicalQueryResult {
                hits: 0,
                returned: 0,
                rows_checked: 0,
                pages_touched: 0,
                hierarchy_lookups: 0,
                rows: Vec::new(),
            });
        };
        let first_physical = self.first_physical_after(after_row_id);
        let (physical_ids, stats) =
            self.engine
                .query_row_ids_page_from(&encoded, limit, first_physical);
        let mut rows = Vec::with_capacity(physical_ids.len());
        for physical in physical_ids {
            let decoded = self.decode_physical_projection(physical, &projection)?;
            let mut values = Vec::with_capacity(projection.len());
            for (slot, &column) in projection.iter().enumerate() {
                values.push(NamedValue {
                    column: self.schema.columns[column].name.clone(),
                    value: decoded[slot].clone(),
                });
            }
            let row_id = self.row_ids.logical(physical).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "query returned unmapped row ID")
            })?;
            rows.push(LogicalRow { row_id, values });
        }
        Ok(LogicalQueryResult {
            hits: stats.hits,
            returned: rows.len(),
            rows_checked: stats.rows_checked,
            pages_touched: stats.pages_touched,
            hierarchy_lookups: stats.hierarchy_lookups,
            rows,
        })
    }

    /// Stream exact equality candidates while testing one numeric range directly against the
    /// canonical token. Dictionary text is borrowed from mmap and parsed in place, avoiding a
    /// per-candidate String allocation and avoiding materializing unrelated columns.
    pub(crate) fn scan_numeric_range_candidates<F>(
        &self,
        predicates: &[LogicalPredicate],
        column: usize,
        gte: Option<&str>,
        lte: Option<&str>,
        batch_rows: usize,
        mut visit: F,
    ) -> io::Result<Option<QueryStats>>
    where
        F: FnMut(u64, bool) -> io::Result<()>,
    {
        if column >= self.schema.columns.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "numeric range column is out of bounds",
            ));
        }

        enum Bounds {
            Unsigned(Option<u64>, Option<u64>),
            Signed(Option<i64>, Option<i64>),
        }

        let rank_bounds = self.numeric_orders[column]
            .as_ref()
            .map(|order| order.rank_bounds(gte, lte))
            .transpose()?;
        let fallback_bounds = if rank_bounds.is_none() {
            Some(match self.schema.columns[column].logical_type {
                crate::LogicalType::Unsigned => Bounds::Unsigned(
                    gte.map(|value| {
                        value.parse::<u64>().map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("invalid canonical unsigned range bound: {error}"),
                            )
                        })
                    }).transpose()?,
                    lte.map(|value| {
                        value.parse::<u64>().map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("invalid canonical unsigned range bound: {error}"),
                            )
                        })
                    }).transpose()?,
                ),
                crate::LogicalType::Signed => Bounds::Signed(
                    gte.map(|value| {
                        value.parse::<i64>().map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("invalid canonical signed range bound: {error}"),
                            )
                        })
                    }).transpose()?,
                    lte.map(|value| {
                        value.parse::<i64>().map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("invalid canonical signed range bound: {error}"),
                            )
                        })
                    }).transpose()?,
                ),
                ref other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("numeric range stream requires signed/unsigned column, got {other:?}"),
                    ))
                }
            })
        } else {
            None
        };

        let Some(encoded) = self.encoded_predicates(predicates)? else {
            return Ok(Some(QueryStats::default()));
        };

        self.engine.scan_row_ids(&encoded, batch_rows, 0, |physical_ids| {
            for &physical in physical_ids {
                let raw_token = self.engine.row_value(physical, column).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "candidate row is missing numeric range token",
                    )
                })?;
                let token = u32::try_from(raw_token).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "numeric range token exceeds u32",
                    )
                })?;
                let matches = if let (Some(order), Some((lo, hi))) =
                    (self.numeric_orders[column].as_ref(), rank_bounds)
                {
                    order.token_in_rank_bounds(token, lo, hi)
                } else {
                    match self.dictionaries[column].decode(token) {
                        Some(DecodedValue::Null) | None => false,
                        Some(DecodedValue::Text(text)) => match fallback_bounds.as_ref().unwrap() {
                            Bounds::Unsigned(lo, hi) => {
                                let value = text.parse::<u64>().map_err(|error| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        format!("invalid canonical unsigned value: {error}"),
                                    )
                                })?;
                                lo.as_ref().map_or(true, |bound| value >= *bound)
                                    && hi.as_ref().map_or(true, |bound| value <= *bound)
                            }
                            Bounds::Signed(lo, hi) => {
                                let value = text.parse::<i64>().map_err(|error| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        format!("invalid canonical signed value: {error}"),
                                    )
                                })?;
                                lo.as_ref().map_or(true, |bound| value >= *bound)
                                    && hi.as_ref().map_or(true, |bound| value <= *bound)
                            }
                        },
                    }
                };
                let row_id = self.row_ids.logical(physical).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "query returned unmapped row ID",
                    )
                })?;
                visit(row_id, matches)?;
            }
            Ok(())
        })
    }

    /// Stream logical row IDs for a fully covered exact equality conjunction without decoding
    /// canonical column values.
    pub(crate) fn scan_row_ids<F>(
        &self,
        predicates: &[LogicalPredicate],
        batch_rows: usize,
        mut visit: F,
    ) -> io::Result<Option<QueryStats>>
    where
        F: FnMut(u64) -> io::Result<()>,
    {
        let Some(encoded) = self.encoded_predicates(predicates)? else {
            return Ok(Some(QueryStats::default()));
        };
        self.engine.scan_row_ids(&encoded, batch_rows, 0, |physical_ids| {
            for &physical in physical_ids {
                let row_id = self.row_ids.logical(physical).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "query returned unmapped row ID")
                })?;
                visit(row_id)?;
            }
            Ok(())
        })
    }

    /// Stream every row ID proven by a fully covered exact equality conjunction. The engine
    /// plans the conjunction once, keeps only a bounded posting batch in memory, and this layer
    /// decodes only the requested projection for each candidate.
    pub(crate) fn scan_values<F>(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        batch_rows: usize,
        mut visit: F,
    ) -> io::Result<Option<QueryStats>>
    where
        F: FnMut(LogicalRow) -> io::Result<()>,
    {
        let projection = self.projection(select)?;
        let Some(encoded) = self.encoded_predicates(predicates)? else {
            return Ok(Some(QueryStats::default()));
        };

        self.engine.scan_row_ids(&encoded, batch_rows, 0, |physical_ids| {
            for &physical in physical_ids {
                let decoded = self.decode_physical_projection(physical, &projection)?;
                let mut values = Vec::with_capacity(projection.len());
                for (slot, &column) in projection.iter().enumerate() {
                    values.push(NamedValue {
                        column: self.schema.columns[column].name.clone(),
                        value: decoded[slot].clone(),
                    });
                }
                let row_id = self.row_ids.logical(physical).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "query returned unmapped row ID")
                })?;
                visit(LogicalRow { row_id, values })?;
            }
            Ok(())
        })
    }

    /// Compatibility helper for non-null equality predicates.
    pub fn query_eq(
        &self,
        predicates: &[(String, String)],
        select: Option<&[String]>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        let predicates: Vec<_> = predicates
            .iter()
            .map(|(column, value)| LogicalPredicate {
                column: column.clone(),
                value: Some(value.clone()),
            })
            .collect();
        self.query_values(&predicates, select, limit)
    }
}

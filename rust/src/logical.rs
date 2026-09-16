use crate::{
    read_schema, resolve_dataset_root, DatasetSchema, DecodedValue, Dictionary, Engine, Manifest,
    Predicate, RowIdMap,
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

pub fn dictionary_filename(column: usize) -> String {
    format!("c{column:04}.dict")
}

pub struct LogicalDataset {
    root: PathBuf,
    schema: DatasetSchema,
    dictionaries: Vec<Dictionary>,
    engine: Engine,
    row_ids: RowIdMap,
    rows: u64,
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
        let engine = Engine::open(&root)?;
        let row_ids = RowIdMap::open_optional(&root, manifest.rows)?;
        Ok(Self {
            root,
            schema,
            dictionaries,
            engine,
            row_ids,
            rows: manifest.rows,
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

    pub fn query_values(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
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
        let (physical_ids, stats) = self.engine.query_row_ids(&encoded, limit);
        let mut rows = Vec::with_capacity(physical_ids.len());
        for physical in physical_ids {
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

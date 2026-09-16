use crate::{read_schema, resolve_dataset_root, DatasetSchema, DecodedValue, Dictionary, Engine, Manifest, Predicate};
use serde::Serialize;
use std::{fs, io, path::{Path, PathBuf}};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NamedValue {
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
        Ok(Self {
            root,
            schema,
            dictionaries,
            engine,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn schema(&self) -> &DatasetSchema {
        &self.schema
    }

    fn encoded_predicates(&self, predicates: &[(String, String)]) -> io::Result<Option<Vec<Predicate>>> {
        let mut out = Vec::with_capacity(predicates.len());
        for (name, value) in predicates {
            let index = self.schema.column_index(name).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("unknown column {name}"))
            })?;
            let normalized = self.schema.columns[index].normalization.apply(value);
            let Some(token) = self.dictionaries[index].token(&normalized) else {
                return Ok(None);
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
                        io::Error::new(io::ErrorKind::InvalidInput, format!("unknown selected column {name}"))
                    })
                })
                .collect(),
        }
    }

    pub fn query_eq(
        &self,
        predicates: &[(String, String)],
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
        let (row_ids, stats) = self.engine.query_row_ids(&encoded, limit);
        let mut rows = Vec::with_capacity(row_ids.len());
        for row_id in row_ids {
            let tokens = self.engine.row(row_id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "query returned invalid row id")
            })?;
            let mut values = Vec::with_capacity(projection.len());
            for &column in &projection {
                let token = u32::try_from(tokens[column]).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "row token exceeds u32")
                })?;
                let decoded = self.dictionaries[column].decode(token).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "row token absent from dictionary")
                })?;
                let value = match decoded {
                    DecodedValue::Null => None,
                    DecodedValue::Text(text) => Some(text.to_owned()),
                };
                values.push(NamedValue {
                    column: self.schema.columns[column].name.clone(),
                    value,
                });
            }
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
}

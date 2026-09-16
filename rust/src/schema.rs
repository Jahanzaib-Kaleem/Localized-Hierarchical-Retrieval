use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Write},
    path::Path,
};

pub const SCHEMA_FORMAT: &str = "LHR-SCHEMA/1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogicalType {
    Text,
    Unsigned,
    Signed,
    Boolean,
    Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Normalization {
    None,
    Trim,
    Lowercase,
    TrimLowercase,
}

impl Normalization {
    pub fn apply(&self, input: &str) -> String {
        match self {
            Self::None => input.to_owned(),
            Self::Trim => input.trim().to_owned(),
            Self::Lowercase => input.to_lowercase(),
            Self::TrimLowercase => input.trim().to_lowercase(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnSchema {
    pub name: String,
    #[serde(default = "default_logical_type")]
    pub logical_type: LogicalType,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default = "default_normalization")]
    pub normalization: Normalization,
    /// Raw input literals treated as NULL before normalization. This is explicit so an empty
    /// string can remain a real value unless the schema intentionally lists it here.
    #[serde(default)]
    pub null_values: Vec<String>,
}

impl ColumnSchema {
    pub fn is_null_literal(&self, raw: &str) -> bool {
        self.nullable && self.null_values.iter().any(|x| x == raw)
    }

    /// Convert external text into the exact dictionary representation used by this column.
    /// Numeric/boolean types are canonicalized so equivalent spellings receive one token.
    pub fn canonicalize(&self, raw: &str) -> io::Result<String> {
        let normalized = self.normalization.apply(raw);
        match self.logical_type {
            LogicalType::Text | LogicalType::Timestamp => Ok(normalized),
            LogicalType::Unsigned => normalized.parse::<u64>().map(|x| x.to_string()).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("column {} expects unsigned integer: {e}", self.name),
                )
            }),
            LogicalType::Signed => normalized.parse::<i64>().map(|x| x.to_string()).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("column {} expects signed integer: {e}", self.name),
                )
            }),
            LogicalType::Boolean => match normalized.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Ok("true".into()),
                "false" | "0" => Ok("false".into()),
                other => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("column {} expects boolean, got {other:?}", self.name),
                )),
            },
        }
    }
}

fn default_logical_type() -> LogicalType {
    LogicalType::Text
}

fn default_normalization() -> Normalization {
    Normalization::None
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasetSchema {
    pub format: String,
    pub columns: Vec<ColumnSchema>,
}

impl DatasetSchema {
    pub fn new(columns: Vec<ColumnSchema>) -> io::Result<Self> {
        let schema = Self {
            format: SCHEMA_FORMAT.into(),
            columns,
        };
        schema.validate()?;
        Ok(schema)
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.format != SCHEMA_FORMAT {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported schema format"));
        }
        if self.columns.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "schema has no columns"));
        }
        let mut names = HashSet::new();
        for column in &self.columns {
            if column.name.trim().is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "column name is empty"));
            }
            if !names.insert(column.name.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate column name {}", column.name),
                ));
            }
            if !column.nullable && !column.null_values.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("non-nullable column {} declares null_values", column.name),
                ));
            }
        }
        Ok(())
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|x| x.name == name)
    }
}

pub fn read_schema_file(path: impl AsRef<Path>) -> io::Result<DatasetSchema> {
    let schema: DatasetSchema = serde_json::from_slice(&fs::read(path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    schema.validate()?;
    Ok(schema)
}

pub fn read_schema(root: impl AsRef<Path>) -> io::Result<DatasetSchema> {
    read_schema_file(root.as_ref().join("schema.json"))
}

pub fn write_schema(root: impl AsRef<Path>, schema: &DatasetSchema) -> io::Result<()> {
    schema.validate()?;
    let root = root.as_ref();
    fs::create_dir_all(root)?;
    let path = root.join("schema.json");
    let tmp = root.join(format!(".schema.json.tmp-{}", std::process::id()));
    let data = serde_json::to_vec_pretty(schema)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io,
    path::{Component, Path},
};

pub const STORAGE_LAYOUT_FILE: &str = "storage.json";
pub const STORAGE_LAYOUT_FORMAT: &str = "LHR-STORAGE/1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StorageMode {
    Native,
    HybridParquet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParquetPayloadFile {
    /// Dataset-relative path. Absolute paths and parent traversal are rejected.
    pub file: String,
    /// First physical row covered by this file.
    pub row_start: u64,
    /// Number of physical rows covered by this file.
    pub rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageLayout {
    pub format: String,
    pub mode: StorageMode,
    /// Logical columns kept in the mmap-friendly LHR token row store.
    pub hot_columns: Vec<usize>,
    /// Logical columns materialized from Parquet token payloads.
    pub cold_columns: Vec<usize>,
    /// Ordered Parquet payload files for HybridParquet mode.
    #[serde(default)]
    pub parquet_payloads: Vec<ParquetPayloadFile>,
    /// Target Parquet row-group size used when LHR owns the payload files.
    #[serde(default = "default_row_group_rows")]
    pub row_group_rows: usize,
}

fn default_row_group_rows() -> usize {
    65_536
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn safe_relative_file(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        && !path.as_os_str().is_empty()
        && path.components().all(|component| {
            matches!(component, Component::Normal(_))
        })
}

impl StorageLayout {
    /// Backwards-compatible layout for every existing LHR/1 dataset.
    ///
    /// `storage.json` is optional. If it is absent, readers must behave exactly as before: every
    /// logical column lives in the native mmap-friendly canonical token store.
    pub fn native(columns: usize) -> Self {
        Self {
            format: STORAGE_LAYOUT_FORMAT.into(),
            mode: StorageMode::Native,
            hot_columns: (0..columns).collect(),
            cold_columns: Vec::new(),
            parquet_payloads: Vec::new(),
            row_group_rows: default_row_group_rows(),
        }
    }

    pub fn hybrid(
        hot_columns: Vec<usize>,
        cold_columns: Vec<usize>,
        parquet_payloads: Vec<ParquetPayloadFile>,
    ) -> Self {
        Self {
            format: STORAGE_LAYOUT_FORMAT.into(),
            mode: StorageMode::HybridParquet,
            hot_columns,
            cold_columns,
            parquet_payloads,
            row_group_rows: default_row_group_rows(),
        }
    }

    pub fn validate(&self, columns: usize, rows: u64) -> io::Result<()> {
        if self.format != STORAGE_LAYOUT_FORMAT {
            return Err(invalid(format!(
                "unsupported storage layout {:?}; expected {STORAGE_LAYOUT_FORMAT}",
                self.format
            )));
        }
        if self.row_group_rows == 0 {
            return Err(invalid("storage row_group_rows must be > 0"));
        }

        let mut seen = BTreeSet::new();
        for &column in self.hot_columns.iter().chain(&self.cold_columns) {
            if column >= columns {
                return Err(invalid(format!(
                    "storage layout references column {column}, but dataset has {columns} columns"
                )));
            }
            if !seen.insert(column) {
                return Err(invalid(format!(
                    "storage layout assigns column {column} more than once"
                )));
            }
        }
        if seen.len() != columns || seen.iter().copied().ne(0..columns) {
            return Err(invalid(
                "hot_columns and cold_columns must partition every logical column exactly once",
            ));
        }

        match self.mode {
            StorageMode::Native => {
                if !self.cold_columns.is_empty() || !self.parquet_payloads.is_empty() {
                    return Err(invalid(
                        "native storage cannot declare cold columns or Parquet payloads",
                    ));
                }
                if self.hot_columns.len() != columns {
                    return Err(invalid("native storage must keep every column hot"));
                }
            }
            StorageMode::HybridParquet => {
                if self.cold_columns.is_empty() {
                    return Err(invalid(
                        "hybrid_parquet storage requires at least one cold column",
                    ));
                }
                if rows > 0 && self.parquet_payloads.is_empty() {
                    return Err(invalid(
                        "hybrid_parquet storage requires Parquet payloads for a non-empty dataset",
                    ));
                }
                let mut expected_start = 0u64;
                for payload in &self.parquet_payloads {
                    if !safe_relative_file(&payload.file) {
                        return Err(invalid(format!(
                            "unsafe Parquet payload path {:?}",
                            payload.file
                        )));
                    }
                    if payload.rows == 0 {
                        return Err(invalid(format!(
                            "Parquet payload {:?} has zero rows",
                            payload.file
                        )));
                    }
                    if payload.row_start != expected_start {
                        return Err(invalid(format!(
                            "Parquet payload {:?} starts at {}, expected {}",
                            payload.file, payload.row_start, expected_start
                        )));
                    }
                    expected_start = expected_start
                        .checked_add(payload.rows)
                        .ok_or_else(|| invalid("Parquet payload row range overflow"))?;
                }
                if expected_start != rows {
                    return Err(invalid(format!(
                        "Parquet payloads cover {expected_start} rows, dataset has {rows}"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Read the optional storage contract for a physical generation.
///
/// Missing `storage.json` deliberately means the legacy all-hot native layout, preserving every
/// existing LHR/1 generation without migration or behavior changes.
pub fn read_storage_layout(
    root: impl AsRef<Path>,
    columns: usize,
    rows: u64,
) -> io::Result<StorageLayout> {
    let path = root.as_ref().join(STORAGE_LAYOUT_FILE);
    let layout = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<StorageLayout>(&bytes)
            .map_err(|error| invalid(format!("invalid {}: {error}", path.display())))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => StorageLayout::native(columns),
        Err(error) => return Err(error),
    };
    layout.validate(columns, rows)?;
    Ok(layout)
}

pub fn write_storage_layout(root: impl AsRef<Path>, layout: &StorageLayout) -> io::Result<()> {
    let root = root.as_ref();
    fs::create_dir_all(root)?;
    let path = root.join(STORAGE_LAYOUT_FILE);
    let tmp = root.join(format!("{STORAGE_LAYOUT_FILE}.tmp-{}", std::process::id()));
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(layout)
            .map_err(|error| invalid(format!("serialize storage layout: {error}")))?,
    )?;
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_layout_is_legacy_native() {
        let dir = tempfile::tempdir().unwrap();
        let layout = read_storage_layout(dir.path(), 3, 10).unwrap();
        assert_eq!(layout, StorageLayout::native(3));
    }

    #[test]
    fn hybrid_columns_must_partition_schema() {
        let layout = StorageLayout::hybrid(
            vec![0, 1],
            vec![1, 2],
            vec![ParquetPayloadFile {
                file: "payload/cold-000000.parquet".into(),
                row_start: 0,
                rows: 10,
            }],
        );
        assert!(layout.validate(3, 10).is_err());
    }

    #[test]
    fn hybrid_payload_ranges_must_be_contiguous() {
        let layout = StorageLayout::hybrid(
            vec![0],
            vec![1],
            vec![
                ParquetPayloadFile {
                    file: "payload/cold-000000.parquet".into(),
                    row_start: 0,
                    rows: 5,
                },
                ParquetPayloadFile {
                    file: "payload/cold-000001.parquet".into(),
                    row_start: 6,
                    rows: 4,
                },
            ],
        );
        assert!(layout.validate(2, 10).is_err());
    }

    #[test]
    fn hybrid_layout_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::hybrid(
            vec![0, 2],
            vec![1, 3],
            vec![ParquetPayloadFile {
                file: "payload/cold-000000.parquet".into(),
                row_start: 0,
                rows: 10,
            }],
        );
        layout.validate(4, 10).unwrap();
        write_storage_layout(dir.path(), &layout).unwrap();
        assert_eq!(read_storage_layout(dir.path(), 4, 10).unwrap(), layout);
    }

    #[test]
    fn payload_paths_cannot_escape_generation() {
        let layout = StorageLayout::hybrid(
            vec![0],
            vec![1],
            vec![ParquetPayloadFile {
                file: "../outside.parquet".into(),
                row_start: 0,
                rows: 10,
            }],
        );
        assert!(layout.validate(2, 10).is_err());
    }
}

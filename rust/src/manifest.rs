use serde::{Deserialize, Serialize};

/// Repository-wide dataset format supported by this release line.
///
/// Any incompatible on-disk change must use a new identifier (for example `LHR/2`) rather than
/// silently accepting a vaguely matching prefix. Individual binary structures also retain their
/// own magic/version markers.
pub const DATASET_FORMAT: &str = "LHR/1";

pub fn ensure_supported_format(format: &str) -> Result<(), String> {
    if format == DATASET_FORMAT {
        Ok(())
    } else {
        Err(format!("unsupported dataset format {format:?}; this build supports {DATASET_FORMAT}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub file: String,
    pub row_start: u64,
    pub rows: u64,
    pub first_page: u32,
}

fn default_sparse() -> String { "sparse".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HierarchyMeta {
    pub file: String,
    pub columns: Vec<usize>,
    pub entries: u64,
    #[serde(default = "default_sparse")]
    pub kind: String,
    #[serde(default)]
    pub keyspace: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub rows: u64,
    pub columns: usize,
    pub page_rows: usize,
    pub pages: u32,
    pub cardinalities: Vec<u64>,
    pub segments: Vec<SegmentMeta>,
    pub hierarchies: Vec<HierarchyMeta>,
}

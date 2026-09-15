use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub file: String,
    pub row_start: u64,
    pub rows: u64,
    pub first_page: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HierarchyMeta {
    pub file: String,
    pub columns: Vec<usize>,
    pub entries: u64,
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

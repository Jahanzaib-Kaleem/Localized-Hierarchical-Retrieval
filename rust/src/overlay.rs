use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

pub const OVERLAY_FILE: &str = "overlay.json";
pub const VISIBILITY_FILE: &str = "visibility.bin";
pub const DELTAS_DIR: &str = "deltas";
pub const OVERLAY_FORMAT: &str = "LHR-OVERLAY/1";
const VIS_MAGIC: &[u8; 8] = b"LHRVIS01";
const VIS_HEADER: usize = 16;
const VIS_RECORD: usize = 16;
pub const DELETED_LAYER: u32 = u32::MAX;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeltaLayerMeta {
    pub id: u32,
    pub path: String,
    pub rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlayCatalog {
    pub format: String,
    pub deltas: Vec<DeltaLayerMeta>,
    pub visible_rows: u64,
    pub max_row_id: Option<u64>,
}

impl OverlayCatalog {
    pub fn empty(visible_rows: u64, max_row_id: Option<u64>) -> Self {
        Self {
            format: OVERLAY_FORMAT.into(),
            deltas: Vec::new(),
            visible_rows,
            max_row_id,
        }
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.format != OVERLAY_FORMAT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported overlay format",
            ));
        }
        let mut last = None;
        for layer in &self.deltas {
            if layer.id == 0 || last.is_some_and(|x| layer.id <= x) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "delta layer IDs must be strictly increasing and non-zero",
                ));
            }
            let path = Path::new(&layer.path);
            if path.is_absolute()
                || path.components().any(|part| {
                    matches!(part, std::path::Component::ParentDir | std::path::Component::RootDir)
                })
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "delta layer path escapes the generation root",
                ));
            }
            last = Some(layer.id);
        }
        Ok(())
    }

    pub fn next_layer_id(&self) -> io::Result<u32> {
        self.deltas
            .last()
            .map(|x| x.id)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "delta layer id overflow"))
    }
}

pub fn read_overlay(root: impl AsRef<Path>, rows: u64, max_row_id: Option<u64>) -> io::Result<OverlayCatalog> {
    let root = root.as_ref();
    let path = root.join(OVERLAY_FILE);
    if !path.exists() {
        return Ok(OverlayCatalog::empty(rows, max_row_id));
    }
    let overlay: OverlayCatalog = serde_json::from_slice(&fs::read(path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    overlay.validate()?;
    Ok(overlay)
}

pub fn write_overlay(root: impl AsRef<Path>, overlay: &OverlayCatalog) -> io::Result<()> {
    overlay.validate()?;
    let root = root.as_ref();
    let path = root.join(OVERLAY_FILE);
    let tmp = root.join(format!(".{OVERLAY_FILE}.tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(overlay)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityTarget {
    Layer(u32),
    Deleted,
}

impl VisibilityTarget {
    fn raw(self) -> u32 {
        match self {
            Self::Layer(layer) => layer,
            Self::Deleted => DELETED_LAYER,
        }
    }

    fn from_raw(raw: u32) -> io::Result<Self> {
        if raw == DELETED_LAYER {
            Ok(Self::Deleted)
        } else if raw == 0 {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "visibility override cannot target base layer 0",
            ))
        } else {
            Ok(Self::Layer(raw))
        }
    }
}

pub enum VisibilityMap {
    Empty,
    Mapped { map: Mmap, count: u64 },
}

impl VisibilityMap {
    pub fn open_optional(root: impl AsRef<Path>) -> io::Result<Self> {
        let path = root.as_ref().join(VISIBILITY_FILE);
        if !path.exists() {
            return Ok(Self::Empty);
        }
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < VIS_HEADER || &map[..8] != VIS_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad visibility map header",
            ));
        }
        let count = u64::from_le_bytes(map[8..16].try_into().unwrap());
        let expected = VIS_HEADER
            .checked_add(
                usize::try_from(count)
                    .ok()
                    .and_then(|x| x.checked_mul(VIS_RECORD))
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "visibility map overflow"))?,
            )
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "visibility map overflow"))?;
        if map.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "visibility map length mismatch",
            ));
        }
        let out = Self::Mapped { map, count };
        let mut previous = None;
        for index in 0..count {
            let (row_id, _) = out.record(index).unwrap();
            if previous.is_some_and(|x| row_id <= x) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "visibility row IDs are not strictly increasing",
                ));
            }
            previous = Some(row_id);
        }
        Ok(out)
    }

    pub fn len(&self) -> u64 {
        match self {
            Self::Empty => 0,
            Self::Mapped { count, .. } => *count,
        }
    }

    fn record(&self, index: u64) -> Option<(u64, VisibilityTarget)> {
        let Self::Mapped { map, count } = self else {
            return None;
        };
        if index >= *count {
            return None;
        }
        let offset = VIS_HEADER + usize::try_from(index).ok()? * VIS_RECORD;
        let row_id = u64::from_le_bytes(map[offset..offset + 8].try_into().unwrap());
        let raw = u32::from_le_bytes(map[offset + 8..offset + 12].try_into().unwrap());
        let target = VisibilityTarget::from_raw(raw).ok()?;
        Some((row_id, target))
    }

    pub fn target(&self, row_id: u64) -> Option<VisibilityTarget> {
        let mut lo = 0u64;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (id, target) = self.record(mid).unwrap();
            match id.cmp(&row_id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(target),
            }
        }
        None
    }

    pub fn to_map(&self) -> BTreeMap<u64, VisibilityTarget> {
        let mut out = BTreeMap::new();
        for index in 0..self.len() {
            let (row_id, target) = self.record(index).unwrap();
            out.insert(row_id, target);
        }
        out
    }
}

pub fn write_visibility(
    root: impl AsRef<Path>,
    entries: &BTreeMap<u64, VisibilityTarget>,
) -> io::Result<()> {
    let root = root.as_ref();
    let path = root.join(VISIBILITY_FILE);
    if entries.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    let tmp = root.join(format!(".{VISIBILITY_FILE}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut writer = BufWriter::new(File::create(&tmp)?);
        writer.write_all(VIS_MAGIC)?;
        writer.write_all(&(entries.len() as u64).to_le_bytes())?;
        for (&row_id, &target) in entries {
            writer.write_all(&row_id.to_le_bytes())?;
            writer.write_all(&target.raw().to_le_bytes())?;
            writer.write_all(&0u32.to_le_bytes())?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub fn delta_path(id: u32) -> PathBuf {
    PathBuf::from(DELTAS_DIR).join(format!("{id:010}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_roundtrip_and_binary_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = BTreeMap::new();
        entries.insert(2, VisibilityTarget::Layer(1));
        entries.insert(9, VisibilityTarget::Deleted);
        entries.insert(17, VisibilityTarget::Layer(4));
        write_visibility(dir.path(), &entries).unwrap();
        let map = VisibilityMap::open_optional(dir.path()).unwrap();
        assert_eq!(map.target(2), Some(VisibilityTarget::Layer(1)));
        assert_eq!(map.target(9), Some(VisibilityTarget::Deleted));
        assert_eq!(map.target(10), None);
        assert_eq!(map.to_map(), entries);
    }
}

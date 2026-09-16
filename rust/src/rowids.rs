use memmap2::Mmap;
use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
};

const MAGIC: &[u8; 8] = b"LHRRID01";
const HEADER: usize = 16;
pub const ROW_IDS_FILE: &str = "rowids.bin";

pub struct RowIdWriter {
    writer: BufWriter<File>,
    expected: u64,
    written: u64,
    last: Option<u64>,
}

impl RowIdWriter {
    pub fn create(path: impl AsRef<Path>, rows: u64) -> io::Result<Self> {
        let mut writer = BufWriter::new(File::create(path)?);
        writer.write_all(MAGIC)?;
        writer.write_all(&rows.to_le_bytes())?;
        Ok(Self {
            writer,
            expected: rows,
            written: 0,
            last: None,
        })
    }

    pub fn push(&mut self, id: u64) -> io::Result<()> {
        if self.written >= self.expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many logical row IDs",
            ));
        }
        if self.last.is_some_and(|last| id <= last) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "logical row IDs must be strictly increasing",
            ));
        }
        self.writer.write_all(&id.to_le_bytes())?;
        self.last = Some(id);
        self.written += 1;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        if self.written != self.expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "logical row ID count mismatch: expected {}, wrote {}",
                    self.expected, self.written
                ),
            ));
        }
        self.writer.flush()?;
        self.writer.get_ref().sync_all()
    }
}

pub enum RowIdMap {
    Identity { rows: u64 },
    Explicit { map: Mmap, rows: u64 },
}

impl RowIdMap {
    pub fn open_optional(root: impl AsRef<Path>, rows: u64) -> io::Result<Self> {
        let path = root.as_ref().join(ROW_IDS_FILE);
        if !path.exists() {
            return Ok(Self::Identity { rows });
        }
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER || &map[..8] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad logical row ID map",
            ));
        }
        let file_rows = u64::from_le_bytes(map[8..16].try_into().unwrap());
        if file_rows != rows {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "logical row ID map row count mismatch",
            ));
        }
        let expected = HEADER
            .checked_add(
                usize::try_from(rows)
                    .ok()
                    .and_then(|x| x.checked_mul(8))
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "row ID map overflow"))?,
            )
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "row ID map overflow"))?;
        if map.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "logical row ID map length mismatch",
            ));
        }
        let out = Self::Explicit { map, rows };
        // Monotonic IDs make reverse lookup deterministic and logarithmic.
        let mut previous = None;
        for physical in 0..rows {
            let id = out.logical(physical).unwrap();
            if previous.is_some_and(|last| id <= last) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "logical row IDs are not strictly increasing",
                ));
            }
            previous = Some(id);
        }
        Ok(out)
    }

    pub fn rows(&self) -> u64 {
        match self {
            Self::Identity { rows } | Self::Explicit { rows, .. } => *rows,
        }
    }

    pub fn logical(&self, physical: u64) -> Option<u64> {
        if physical >= self.rows() {
            return None;
        }
        match self {
            Self::Identity { .. } => Some(physical),
            Self::Explicit { map, .. } => {
                let physical = usize::try_from(physical).ok()?;
                let offset = HEADER + physical * 8;
                Some(u64::from_le_bytes(map[offset..offset + 8].try_into().unwrap()))
            }
        }
    }

    pub fn physical(&self, logical: u64) -> Option<u64> {
        match self {
            Self::Identity { rows } => (logical < *rows).then_some(logical),
            Self::Explicit { .. } => {
                let mut lo = 0u64;
                let mut hi = self.rows();
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    match self.logical(mid).unwrap().cmp(&logical) {
                        std::cmp::Ordering::Less => lo = mid + 1,
                        std::cmp::Ordering::Greater => hi = mid,
                        std::cmp::Ordering::Equal => return Some(mid),
                    }
                }
                None
            }
        }
    }

    pub fn max_id(&self) -> Option<u64> {
        self.rows().checked_sub(1).and_then(|x| self.logical(x))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_map_roundtrips_and_reverse_searches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ROW_IDS_FILE);
        let mut writer = RowIdWriter::create(&path, 3).unwrap();
        writer.push(2).unwrap();
        writer.push(7).unwrap();
        writer.push(11).unwrap();
        writer.finish().unwrap();
        let map = RowIdMap::open_optional(dir.path(), 3).unwrap();
        assert_eq!(map.logical(1), Some(7));
        assert_eq!(map.physical(11), Some(2));
        assert_eq!(map.physical(8), None);
        assert_eq!(map.max_id(), Some(11));
    }
}

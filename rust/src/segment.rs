use crate::read_token_projection;
use memmap2::Mmap;
use std::{
    fs::File,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

const NATIVE_MAGIC: &[u8; 8] = b"LHRSEG01";
const HYBRID_MAGIC: &[u8; 8] = b"LHRHYB01";
const NATIVE_HEADER: usize = 24;
const HYBRID_HEADER: usize = 40;

struct ColdCache {
    row_start: usize,
    values: Vec<Vec<u32>>,
}

enum SegmentStorage {
    Native {
        payload_offset: usize,
    },
    Hybrid {
        payload_offset: usize,
        hot_columns: Vec<usize>,
        hot_position: Vec<Option<usize>>,
        cold_columns: Vec<usize>,
        cold_position: Vec<Option<usize>>,
        cold_file: PathBuf,
        row_group_rows: usize,
        cache: Mutex<Option<ColdCache>>,
    },
}

pub struct Segment {
    map: Mmap,
    rows: usize,
    cols: usize,
    width: usize,
    storage: SegmentStorage,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn checked_payload_len(rows: usize, cols: usize, width: usize) -> io::Result<usize> {
    rows.checked_mul(cols)
        .and_then(|value| value.checked_mul(width))
        .ok_or_else(|| invalid("segment size overflow"))
}

fn read_u32(map: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(map[offset..offset + 4].try_into().unwrap())
}

fn read_token(map: &[u8], offset: usize, width: usize) -> u64 {
    let bytes = &map[offset..offset + width];
    match width {
        1 => bytes[0] as u64,
        2 => u16::from_le_bytes(bytes.try_into().unwrap()) as u64,
        4 => u32::from_le_bytes(bytes.try_into().unwrap()) as u64,
        8 => u64::from_le_bytes(bytes.try_into().unwrap()),
        _ => unreachable!(),
    }
}

fn validate_partition(cols: usize, hot: &[usize], cold: &[usize]) -> io::Result<()> {
    let mut seen = vec![false; cols];
    for &column in hot.iter().chain(cold) {
        if column >= cols || seen[column] {
            return Err(invalid("hybrid segment column partition is invalid"));
        }
        seen[column] = true;
    }
    if seen.iter().any(|seen| !seen) {
        return Err(invalid(
            "hybrid segment hot/cold columns must cover every logical column",
        ));
    }
    Ok(())
}

fn safe_sidecar_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

impl Segment {
    pub fn write(
        path: impl AsRef<Path>,
        rows: u64,
        cols: u32,
        width: u32,
        data: &[u8],
    ) -> io::Result<()> {
        let rows = usize::try_from(rows)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "row count exceeds usize"))?;
        let cols = cols as usize;
        let width = width as usize;
        if !matches!(width, 1 | 2 | 4 | 8) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported token width",
            ));
        }
        let expected = checked_payload_len(rows, cols, width)?;
        if data.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "payload length mismatch",
            ));
        }
        let mut file = File::create(path)?;
        file.write_all(NATIVE_MAGIC)?;
        file.write_all(&(rows as u64).to_le_bytes())?;
        file.write_all(&(cols as u32).to_le_bytes())?;
        file.write_all(&(width as u32).to_le_bytes())?;
        file.write_all(data)?;
        file.sync_all()
    }

    /// Write a logical segment whose hot columns remain fixed-width/mmap-friendly and whose cold
    /// token columns live in a same-directory Parquet sidecar.
    pub fn write_hybrid(
        path: impl AsRef<Path>,
        rows: u64,
        logical_cols: u32,
        width: u32,
        hot_columns: &[usize],
        cold_columns: &[usize],
        row_group_rows: usize,
        cold_file_name: &str,
        hot_data: &[u8],
    ) -> io::Result<()> {
        if width != 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hybrid segments currently require u32 token width",
            ));
        }
        if hot_columns.is_empty() || cold_columns.is_empty() || row_group_rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hybrid segment requires non-empty hot/cold columns and row_group_rows > 0",
            ));
        }
        if !safe_sidecar_name(cold_file_name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hybrid Parquet sidecar must be a plain file name",
            ));
        }
        let rows = usize::try_from(rows)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "row count exceeds usize"))?;
        let logical_cols = logical_cols as usize;
        validate_partition(logical_cols, hot_columns, cold_columns)?;
        let expected = checked_payload_len(rows, hot_columns.len(), width as usize)?;
        if hot_data.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hybrid hot payload length mismatch",
            ));
        }
        let name = cold_file_name.as_bytes();
        let name_len = u32::try_from(name.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "hybrid sidecar name is too long")
        })?;
        let hot_count = u32::try_from(hot_columns.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many hot columns"))?;
        let cold_count = u32::try_from(cold_columns.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many cold columns"))?;
        let row_group_rows = u32::try_from(row_group_rows).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "row_group_rows exceeds u32")
        })?;

        let mut file = File::create(path)?;
        file.write_all(HYBRID_MAGIC)?;
        file.write_all(&(rows as u64).to_le_bytes())?;
        file.write_all(&(logical_cols as u32).to_le_bytes())?;
        file.write_all(&width.to_le_bytes())?;
        file.write_all(&hot_count.to_le_bytes())?;
        file.write_all(&cold_count.to_le_bytes())?;
        file.write_all(&row_group_rows.to_le_bytes())?;
        file.write_all(&name_len.to_le_bytes())?;
        for &column in hot_columns {
            file.write_all(&(column as u32).to_le_bytes())?;
        }
        for &column in cold_columns {
            file.write_all(&(column as u32).to_le_bytes())?;
        }
        file.write_all(name)?;
        file.write_all(hot_data)?;
        file.sync_all()
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < NATIVE_HEADER {
            return Err(invalid("bad LHR segment"));
        }

        if &map[..8] == NATIVE_MAGIC {
            let rows = u64::from_le_bytes(map[8..16].try_into().unwrap()) as usize;
            let cols = read_u32(&map, 16) as usize;
            let width = read_u32(&map, 20) as usize;
            if !matches!(width, 1 | 2 | 4 | 8) {
                return Err(invalid("unsupported token width"));
            }
            let expected = NATIVE_HEADER
                .checked_add(checked_payload_len(rows, cols, width)?)
                .ok_or_else(|| invalid("segment overflow"))?;
            if map.len() != expected {
                return Err(invalid("segment length mismatch"));
            }
            return Ok(Self {
                map,
                rows,
                cols,
                width,
                storage: SegmentStorage::Native {
                    payload_offset: NATIVE_HEADER,
                },
            });
        }

        if &map[..8] != HYBRID_MAGIC || map.len() < HYBRID_HEADER {
            return Err(invalid("bad LHR segment"));
        }
        let rows = u64::from_le_bytes(map[8..16].try_into().unwrap()) as usize;
        let cols = read_u32(&map, 16) as usize;
        let width = read_u32(&map, 20) as usize;
        let hot_count = read_u32(&map, 24) as usize;
        let cold_count = read_u32(&map, 28) as usize;
        let row_group_rows = read_u32(&map, 32) as usize;
        let name_len = read_u32(&map, 36) as usize;
        if width != 4 || hot_count == 0 || cold_count == 0 || row_group_rows == 0 {
            return Err(invalid("invalid hybrid segment header"));
        }
        let metadata_bytes = hot_count
            .checked_add(cold_count)
            .and_then(|count| count.checked_mul(4))
            .and_then(|bytes| bytes.checked_add(name_len))
            .ok_or_else(|| invalid("hybrid segment metadata overflow"))?;
        let payload_offset = HYBRID_HEADER
            .checked_add(metadata_bytes)
            .ok_or_else(|| invalid("hybrid segment metadata overflow"))?;
        if payload_offset > map.len() {
            return Err(invalid("truncated hybrid segment metadata"));
        }

        let mut cursor = HYBRID_HEADER;
        let mut hot_columns = Vec::with_capacity(hot_count);
        for _ in 0..hot_count {
            hot_columns.push(read_u32(&map, cursor) as usize);
            cursor += 4;
        }
        let mut cold_columns = Vec::with_capacity(cold_count);
        for _ in 0..cold_count {
            cold_columns.push(read_u32(&map, cursor) as usize);
            cursor += 4;
        }
        validate_partition(cols, &hot_columns, &cold_columns)?;
        let cold_name = std::str::from_utf8(&map[cursor..cursor + name_len])
            .map_err(|_| invalid("hybrid sidecar name is not UTF-8"))?;
        if !safe_sidecar_name(cold_name) {
            return Err(invalid("unsafe hybrid sidecar name"));
        }
        let cold_file = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(cold_name);
        if !cold_file.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("hybrid Parquet sidecar is missing: {}", cold_file.display()),
            ));
        }
        let expected = payload_offset
            .checked_add(checked_payload_len(rows, hot_count, width)?)
            .ok_or_else(|| invalid("hybrid segment overflow"))?;
        if map.len() != expected {
            return Err(invalid("hybrid segment length mismatch"));
        }

        let mut hot_position = vec![None; cols];
        for (position, &column) in hot_columns.iter().enumerate() {
            hot_position[column] = Some(position);
        }
        let mut cold_position = vec![None; cols];
        for (position, &column) in cold_columns.iter().enumerate() {
            cold_position[column] = Some(position);
        }
        Ok(Self {
            map,
            rows,
            cols,
            width,
            storage: SegmentStorage::Hybrid {
                payload_offset,
                hot_columns,
                hot_position,
                cold_columns,
                cold_position,
                cold_file,
                row_group_rows,
                cache: Mutex::new(None),
            },
        })
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn is_hybrid(&self) -> bool {
        matches!(self.storage, SegmentStorage::Hybrid { .. })
    }

    pub fn value_checked(&self, row: usize, col: usize) -> io::Result<Option<u64>> {
        if row >= self.rows || col >= self.cols {
            return Ok(None);
        }
        match &self.storage {
            SegmentStorage::Native { payload_offset } => {
                let offset = payload_offset + (row * self.cols + col) * self.width;
                Ok(Some(read_token(&self.map, offset, self.width)))
            }
            SegmentStorage::Hybrid {
                payload_offset,
                hot_columns,
                hot_position,
                cold_columns,
                cold_position,
                cold_file,
                row_group_rows,
                cache,
            } => {
                if let Some(position) = hot_position[col] {
                    let offset = payload_offset
                        + (row * hot_columns.len() + position) * self.width;
                    return Ok(Some(read_token(&self.map, offset, self.width)));
                }
                let cold_slot = cold_position[col]
                    .ok_or_else(|| invalid("logical column is absent from hybrid partition"))?;
                {
                    let guard = cache
                        .lock()
                        .map_err(|_| invalid("hybrid cold cache mutex poisoned"))?;
                    if let Some(group) = guard.as_ref() {
                        if row >= group.row_start && row < group.row_start + group.values.len() {
                            return Ok(Some(group.values[row - group.row_start][cold_slot] as u64));
                        }
                    }
                }

                let group_start = row / *row_group_rows * *row_group_rows;
                let group_end = (group_start + *row_group_rows).min(self.rows);
                let requested = (group_start..group_end)
                    .map(|value| value as u64)
                    .collect::<Vec<_>>();
                let values = read_token_projection(cold_file, &requested, cold_columns)?;
                let result = values[row - group_start][cold_slot] as u64;
                let mut guard = cache
                    .lock()
                    .map_err(|_| invalid("hybrid cold cache mutex poisoned"))?;
                *guard = Some(ColdCache {
                    row_start: group_start,
                    values,
                });
                Ok(Some(result))
            }
        }
    }

    pub fn value(&self, row: usize, col: usize) -> Option<u64> {
        self.value_checked(row, col)
            .unwrap_or_else(|error| panic!("LHR canonical segment read failed: {error}"))
    }

    pub fn matches(&self, row: usize, predicates: &[(usize, u64)]) -> bool {
        row < self.rows
            && predicates
                .iter()
                .all(|&(column, value)| self.value(row, column) == Some(value))
    }

    pub fn count_page(&self, start: usize, end: usize, predicates: &[(usize, u64)]) -> u64 {
        let mut count = 0;
        for row in start..end.min(self.rows) {
            if self.matches(row, predicates) {
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParquetTokenWriter;

    #[test]
    fn native_roundtrip() {
        let file = tempfile::NamedTempFile::new().unwrap();
        Segment::write(file.path(), 3, 2, 1, &[1, 2, 3, 4, 1, 4]).unwrap();
        let segment = Segment::open(file.path()).unwrap();
        assert_eq!(segment.value(1, 1), Some(4));
        assert!(segment.matches(1, &[(1, 4)]));
        assert_eq!(segment.count_page(0, 3, &[(0, 1)]), 2);
    }

    #[test]
    fn hybrid_segment_reads_hot_and_cold_tokens_transparently() {
        let dir = tempfile::tempdir().unwrap();
        let parquet = dir.path().join("segment-000000.parquet");
        let mut writer = ParquetTokenWriter::create(&parquet, &[1, 3], 2).unwrap();
        writer
            .write_columns(&[vec![10, 11, 12], vec![30, 31, 32]])
            .unwrap();
        writer.finish().unwrap();

        let segment_path = dir.path().join("segment-000000.lhr");
        let hot_tokens = [1u32, 20, 2, 21, 3, 22]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        Segment::write_hybrid(
            &segment_path,
            3,
            4,
            4,
            &[0, 2],
            &[1, 3],
            2,
            "segment-000000.parquet",
            &hot_tokens,
        )
        .unwrap();
        let segment = Segment::open(&segment_path).unwrap();
        assert!(segment.is_hybrid());
        assert_eq!(segment.cols(), 4);
        assert_eq!(segment.value(2, 0), Some(3));
        assert_eq!(segment.value(2, 1), Some(12));
        assert_eq!(segment.value(1, 2), Some(21));
        assert_eq!(segment.value(1, 3), Some(31));
        assert!(segment.matches(1, &[(0, 2), (3, 31)]));
    }
}

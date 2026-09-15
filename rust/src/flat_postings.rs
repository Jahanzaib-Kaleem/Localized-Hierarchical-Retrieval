use memmap2::Mmap;
use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Write},
    path::Path,
};

const MAGIC: &[u8; 8] = b"LHRFLAT1";
const HEADER: usize = 24;
const RECORD: usize = 12;

fn read_record<R: Read>(r: &mut R) -> io::Result<Option<(u64, u32)>> {
    let mut b = [0u8; RECORD];
    let mut got = 0usize;
    while got < b.len() {
        match r.read(&mut b[got..])? {
            0 if got == 0 => return Ok(None),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated flat postings source",
                ))
            }
            n => got += n,
        }
    }
    Ok(Some((
        u64::from_le_bytes(b[..8].try_into().unwrap()),
        u32::from_le_bytes(b[8..].try_into().unwrap()),
    )))
}

pub struct FlatPostingHierarchy {
    map: Mmap,
    rows: u32,
    body: usize,
}

impl FlatPostingHierarchy {
    pub fn estimated_bytes(rows: u64) -> Option<u64> {
        (HEADER as u64).checked_add(rows.checked_mul(RECORD as u64)?)
    }

    pub fn build_from_sorted(
        sorted: impl AsRef<Path>,
        output: impl AsRef<Path>,
        rows: u64,
    ) -> io::Result<()> {
        if rows > u32::MAX as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "flat postings require <= u32::MAX rows",
            ));
        }

        let mut r = BufReader::new(File::open(sorted)?);
        let mut w = BufWriter::new(File::create(output)?);
        w.write_all(MAGIC)?;
        w.write_all(&(rows as u32).to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(&(HEADER as u64).to_le_bytes())?;

        let mut seen = 0u64;
        let mut last: Option<(u64, u32)> = None;
        while let Some((key, row)) = read_record(&mut r)? {
            if let Some(prev) = last {
                if (key, row) <= prev {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "flat postings source is not strictly sorted",
                    ));
                }
            }
            w.write_all(&key.to_le_bytes())?;
            w.write_all(&row.to_le_bytes())?;
            last = Some((key, row));
            seen += 1;
        }
        if seen != rows {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "flat postings row count mismatch",
            ));
        }
        w.flush()
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER || &map[..8] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid flat postings header",
            ));
        }
        let rows = u32::from_le_bytes(map[8..12].try_into().unwrap());
        let body = u64::from_le_bytes(map[16..24].try_into().unwrap()) as usize;
        let expected = HEADER
            .checked_add((rows as usize).checked_mul(RECORD).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "flat postings length overflow")
            })?)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "flat postings length overflow")
            })?;
        if body != HEADER || map.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "flat postings layout mismatch",
            ));
        }
        Ok(Self { map, rows, body })
    }

    fn key_at(&self, index: usize) -> u64 {
        let offset = self.body + index * RECORD;
        u64::from_le_bytes(self.map[offset..offset + 8].try_into().unwrap())
    }

    fn row_at(&self, index: usize) -> u32 {
        let offset = self.body + index * RECORD + 8;
        u32::from_le_bytes(self.map[offset..offset + 4].try_into().unwrap())
    }

    fn lower_bound(&self, key: u64) -> usize {
        let mut lo = 0usize;
        let mut hi = self.rows as usize;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key_at(mid) < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn upper_bound(&self, key: u64) -> usize {
        let mut lo = 0usize;
        let mut hi = self.rows as usize;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key_at(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn bounds(&self, key: u64) -> (usize, usize) {
        let start = self.lower_bound(key);
        if start == self.rows as usize || self.key_at(start) != key {
            return (start, start);
        }
        (start, self.upper_bound(key))
    }

    pub fn row_count(&self, key: u64) -> usize {
        let (start, end) = self.bounds(key);
        end - start
    }

    pub fn rows(&self, key: u64) -> Vec<u32> {
        let (start, end) = self.bounds(key);
        (start..end).map(|i| self.row_at(i)).collect()
    }

    pub fn intersect_rows(&self, key: u64, seed: &[u32]) -> Vec<u32> {
        if seed.is_empty() {
            return Vec::new();
        }
        let (start, end) = self.bounds(key);
        let mut out = Vec::with_capacity(seed.len().min(end - start));
        let mut i = 0usize;
        let mut j = start;
        while i < seed.len() && j < end {
            let row = self.row_at(j);
            match seed[i].cmp(&row) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    out.push(row);
                    i += 1;
                    j += 1;
                }
            }
        }
        out
    }

    pub fn total_rows(&self) -> usize {
        self.rows as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_binary_searches_flat_postings() {
        let d = tempfile::tempdir().unwrap();
        let sorted = d.path().join("sorted");
        let output = d.path().join("flat");
        let mut f = File::create(&sorted).unwrap();
        for (key, row) in [
            (1u64, 2u32),
            (1, 8),
            (4, 0),
            (4, 7),
            (4, 11),
            (10, 3),
        ] {
            f.write_all(&key.to_le_bytes()).unwrap();
            f.write_all(&row.to_le_bytes()).unwrap();
        }
        drop(f);

        FlatPostingHierarchy::build_from_sorted(&sorted, &output, 6).unwrap();
        let x = FlatPostingHierarchy::open(&output).unwrap();
        assert_eq!(x.row_count(0), 0);
        assert_eq!(x.rows(1), vec![2, 8]);
        assert_eq!(x.rows(4), vec![0, 7, 11]);
        assert_eq!(x.rows(10), vec![3]);
        assert_eq!(x.intersect_rows(4, &[0, 2, 7, 9, 11, 13]), vec![0, 7, 11]);
        assert_eq!(x.total_rows(), 6);
        assert_eq!(FlatPostingHierarchy::estimated_bytes(6), Some((HEADER + 6 * RECORD) as u64));
    }
}

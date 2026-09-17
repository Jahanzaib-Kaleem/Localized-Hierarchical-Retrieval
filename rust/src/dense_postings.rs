use memmap2::Mmap;
use std::{fs::File, io::{self, BufReader, BufWriter, Read, Write}, path::Path};

const MAGIC: &[u8; 8] = b"LHRDPOST";
const HEADER: usize = 40;

fn read_record<R: Read>(r: &mut R) -> io::Result<Option<(u64, u32)>> {
    let mut b = [0u8; 12];
    let mut got = 0usize;
    while got < b.len() {
        match r.read(&mut b[got..])? {
            0 if got == 0 => return Ok(None),
            0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated dense postings source")),
            n => got += n,
        }
    }
    Ok(Some((u64::from_le_bytes(b[..8].try_into().unwrap()), u32::from_le_bytes(b[8..].try_into().unwrap()))))
}

pub fn count_unique_sorted(path: impl AsRef<Path>) -> io::Result<u64> {
    let mut r = BufReader::new(File::open(path)?);
    let mut last = None;
    let mut unique = 0u64;
    while let Some((key, _)) = read_record(&mut r)? {
        if last != Some(key) { unique += 1; last = Some(key); }
    }
    Ok(unique)
}

pub struct DensePostingHierarchy {
    map: Mmap,
    keyspace: u64,
    rows: u32,
    offsets: usize,
    body: usize,
}

impl DensePostingHierarchy {
    pub fn estimated_bytes(keyspace: u64, rows: u64) -> Option<u64> {
        (HEADER as u64)
            .checked_add(keyspace.checked_add(1)?.checked_mul(4)?)?
            .checked_add(rows.checked_mul(4)?)
    }

    pub fn build_from_sorted(sorted: impl AsRef<Path>, output: impl AsRef<Path>, keyspace: u64, rows: u64) -> io::Result<()> {
        if rows > u32::MAX as u64 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "dense postings require <= u32::MAX rows")); }
        let sorted = sorted.as_ref();
        let mut r = BufReader::new(File::open(sorted)?);
        let mut w = BufWriter::new(File::create(output)?);
        let body = HEADER as u64 + (keyspace + 1) * 4;
        w.write_all(MAGIC)?;
        w.write_all(&keyspace.to_le_bytes())?;
        w.write_all(&(rows as u32).to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(&(HEADER as u64).to_le_bytes())?;
        w.write_all(&body.to_le_bytes())?;

        // The sorted input is grouped by key. Emit offset[k] = first posting for key k.
        let mut next_offset = 0u64;
        let mut row_index = 0u32;
        let mut current_key: Option<u64> = None;
        while let Some((key, _)) = read_record(&mut r)? {
            if key >= keyspace { return Err(io::Error::new(io::ErrorKind::InvalidData, "posting key outside keyspace")); }
            if current_key != Some(key) {
                while next_offset <= key {
                    w.write_all(&row_index.to_le_bytes())?;
                    next_offset += 1;
                }
                current_key = Some(key);
            }
            row_index = row_index.checked_add(1).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "row posting overflow"))?;
        }
        if row_index as u64 != rows { return Err(io::Error::new(io::ErrorKind::InvalidData, "posting row count mismatch")); }
        while next_offset <= keyspace {
            w.write_all(&row_index.to_le_bytes())?;
            next_offset += 1;
        }

        // Append posting row IDs in the same sorted-by-key order.
        let mut r = BufReader::new(File::open(sorted)?);
        let mut seen = 0u64;
        while let Some((_, row)) = read_record(&mut r)? { w.write_all(&row.to_le_bytes())?; seen += 1; }
        if seen != rows { return Err(io::Error::new(io::ErrorKind::InvalidData, "posting second-pass count mismatch")); }
        w.flush()
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER || &map[..8] != MAGIC { return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid dense postings header")); }
        let keyspace = u64::from_le_bytes(map[8..16].try_into().unwrap());
        let rows = u32::from_le_bytes(map[16..20].try_into().unwrap());
        let offsets = u64::from_le_bytes(map[24..32].try_into().unwrap()) as usize;
        let body = u64::from_le_bytes(map[32..40].try_into().unwrap()) as usize;
        let expected_body = HEADER.checked_add((keyspace as usize + 1).checked_mul(4).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dense offset overflow"))?).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dense offset overflow"))?;
        let expected_len = expected_body.checked_add(rows as usize * 4).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dense body overflow"))?;
        if offsets != HEADER || body != expected_body || map.len() != expected_len { return Err(io::Error::new(io::ErrorKind::InvalidData, "dense postings layout mismatch")); }
        Ok(Self { map, keyspace, rows, offsets, body })
    }

    fn offset(&self, key: u64) -> usize {
        let o = self.offsets + key as usize * 4;
        u32::from_le_bytes(self.map[o..o+4].try_into().unwrap()) as usize
    }
    fn row_at(&self, i: usize) -> u32 {
        let o = self.body + i * 4;
        u32::from_le_bytes(self.map[o..o+4].try_into().unwrap())
    }
    fn bounds(&self, key: u64) -> Option<(usize, usize)> {
        if key >= self.keyspace { return None; }
        let start = self.offset(key);
        let end = self.offset(key + 1);
        Some((start, end - start))
    }
    pub fn row_count(&self, key: u64) -> usize { self.bounds(key).map(|x| x.1).unwrap_or(0) }
    pub fn rows(&self, key: u64) -> Vec<u32> {
        let Some((start, len)) = self.bounds(key) else { return Vec::new(); };
        (start..start+len).map(|i| self.row_at(i)).collect()
    }
    pub fn rows_from(&self, key: u64, first_row: u32, limit: usize) -> Vec<u32> {
        if limit == 0 { return Vec::new(); }
        let Some((start, len)) = self.bounds(key) else { return Vec::new(); };
        let end = start + len;
        let mut lo = start;
        let mut hi = end;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_at(mid) < first_row { lo = mid + 1; } else { hi = mid; }
        }
        (lo..end).take(limit).map(|i| self.row_at(i)).collect()
    }
    pub fn intersect_rows(&self, key: u64, seed: &[u32]) -> Vec<u32> {
        let Some((start, len)) = self.bounds(key) else { return Vec::new(); };
        let mut out = Vec::with_capacity(seed.len().min(len));
        let mut i = 0usize; let mut j = start; let end = start + len;
        while i < seed.len() && j < end {
            let row = self.row_at(j);
            match seed[i].cmp(&row) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => { out.push(row); i += 1; j += 1; }
            }
        }
        out
    }
    pub fn total_rows(&self) -> usize { self.rows as usize }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builds_dense_offsets_with_missing_keys() {
        let d = tempfile::tempdir().unwrap(); let s = d.path().join("s"); let p = d.path().join("p");
        let mut f = File::create(&s).unwrap();
        for (k,r) in [(1u64,0u32),(1,3),(4,1),(4,8),(7,9)] { f.write_all(&k.to_le_bytes()).unwrap(); f.write_all(&r.to_le_bytes()).unwrap(); }
        drop(f);
        DensePostingHierarchy::build_from_sorted(&s,&p,10,5).unwrap();
        let x = DensePostingHierarchy::open(p).unwrap();
        assert_eq!(x.row_count(0),0); assert_eq!(x.rows(1),vec![0,3]); assert_eq!(x.rows(4),vec![1,8]); assert_eq!(x.row_count(9),0);
        assert_eq!(x.rows_from(4, 2, 1), vec![8]);
        assert_eq!(x.rows_from(1, 3, 10), vec![3]);
        assert_eq!(x.intersect_rows(4,&[0,1,3,8,12]),vec![1,8]);
        assert_eq!(x.total_rows(),5);
    }
}

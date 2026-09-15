use memmap2::{Mmap, MmapMut};
use std::{fs::{File, OpenOptions}, io::{self, BufReader, Read}, path::Path};

const MAGIC: &[u8; 8] = b"LHRBIT01";
const HEADER: usize = 32;

pub struct BitmapHierarchy {
    map: Mmap,
    keyspace: u64,
    pages: u32,
    words_per_key: usize,
}

impl BitmapHierarchy {
    pub fn estimated_bytes(keyspace: u64, pages: u32) -> Option<u64> {
        let words = ((pages as u64) + 63) / 64;
        keyspace.checked_mul(words)?.checked_mul(8)?.checked_add(HEADER as u64)
    }

    pub fn build_from_sparse(sparse: impl AsRef<Path>, output: impl AsRef<Path>, keyspace: u64, pages: u32) -> io::Result<()> {
        let bytes = Self::estimated_bytes(keyspace, pages).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bitmap size overflow"))?;
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(output)?;
        file.set_len(bytes)?;
        let mut map = unsafe { MmapMut::map_mut(&file)? };
        map[..8].copy_from_slice(MAGIC);
        map[8..16].copy_from_slice(&keyspace.to_le_bytes());
        map[16..20].copy_from_slice(&pages.to_le_bytes());
        let words_per_key = (pages as usize + 63) / 64;
        map[20..24].copy_from_slice(&(words_per_key as u32).to_le_bytes());
        map[24..32].fill(0);

        let mut reader = BufReader::new(File::open(sparse)?);
        let mut rec = [0u8; 12];
        loop {
            let mut got = 0usize;
            while got < rec.len() {
                match reader.read(&mut rec[got..])? {
                    0 if got == 0 => { map.flush()?; return Ok(()); },
                    0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated sparse hierarchy")),
                    n => got += n,
                }
            }
            let key = u64::from_le_bytes(rec[..8].try_into().unwrap());
            let page = u32::from_le_bytes(rec[8..].try_into().unwrap());
            if key >= keyspace || page >= pages { return Err(io::Error::new(io::ErrorKind::InvalidData, "sparse record outside bitmap bounds")); }
            let word = page as usize / 64;
            let bit = page as usize % 64;
            let off = HEADER + (key as usize * words_per_key + word) * 8;
            let mut v = u64::from_le_bytes(map[off..off+8].try_into().unwrap());
            v |= 1u64 << bit;
            map[off..off+8].copy_from_slice(&v.to_le_bytes());
        }
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER || &map[..8] != MAGIC { return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid LHR bitmap header")); }
        let keyspace = u64::from_le_bytes(map[8..16].try_into().unwrap());
        let pages = u32::from_le_bytes(map[16..20].try_into().unwrap());
        let words_per_key = u32::from_le_bytes(map[20..24].try_into().unwrap()) as usize;
        let expected = Self::estimated_bytes(keyspace, pages).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bitmap size overflow"))? as usize;
        if map.len() != expected || words_per_key != (pages as usize + 63) / 64 { return Err(io::Error::new(io::ErrorKind::InvalidData, "bitmap length mismatch")); }
        Ok(Self { map, keyspace, pages, words_per_key })
    }

    fn word(&self, key: u64, word: usize) -> u64 {
        let off = HEADER + (key as usize * self.words_per_key + word) * 8;
        u64::from_le_bytes(self.map[off..off+8].try_into().unwrap())
    }

    pub fn page_count(&self, key: u64) -> usize {
        if key >= self.keyspace { return 0; }
        (0..self.words_per_key).map(|w| self.word(key, w).count_ones() as usize).sum()
    }

    pub fn pages(&self, key: u64) -> Vec<u32> {
        if key >= self.keyspace { return Vec::new(); }
        let mut out = Vec::with_capacity(self.page_count(key));
        for w in 0..self.words_per_key {
            let mut bits = self.word(key, w);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                let page = w * 64 + bit;
                if page < self.pages as usize { out.push(page as u32); }
                bits &= bits - 1;
            }
        }
        out
    }

    pub fn intersect_pages(&self, key: u64, seed: &[u32]) -> Vec<u32> {
        if key >= self.keyspace { return Vec::new(); }
        seed.iter().copied().filter(|&page| {
            if page >= self.pages { return false; }
            let w = page as usize / 64; let bit = page as usize % 64;
            self.word(key, w) & (1u64 << bit) != 0
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn converts_sparse_and_queries_bits() {
        let d = tempfile::tempdir().unwrap(); let sparse = d.path().join("s.bin"); let bitmap = d.path().join("b.bin");
        let mut f = File::create(&sparse).unwrap();
        for (k,p) in [(1u64,2u32),(1,65),(3,4),(3,127)] { f.write_all(&k.to_le_bytes()).unwrap(); f.write_all(&p.to_le_bytes()).unwrap(); }
        drop(f); BitmapHierarchy::build_from_sparse(&sparse,&bitmap,8,128).unwrap();
        let b=BitmapHierarchy::open(bitmap).unwrap(); assert_eq!(b.page_count(1),2); assert_eq!(b.pages(1),vec![2,65]); assert_eq!(b.intersect_pages(3,&[1,4,5,127]),vec![4,127]);
    }
}

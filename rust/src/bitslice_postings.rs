use memmap2::{Mmap, MmapMut};
use std::{
    fs::{File, OpenOptions},
    io::{self, BufReader, Read},
    path::Path,
};

const MAGIC: &[u8; 8] = b"LHRBSL01";
const HEADER: usize = 48;

fn read_record<R: Read>(r: &mut R) -> io::Result<Option<(u64, u32)>> {
    let mut b = [0u8; 12];
    let mut got = 0usize;
    while got < b.len() {
        match r.read(&mut b[got..])? {
            0 if got == 0 => return Ok(None),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated bit-slice source",
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

fn bits_for_keyspace(keyspace: u64) -> u32 {
    if keyspace <= 1 {
        0
    } else {
        64 - (keyspace - 1).leading_zeros()
    }
}

pub struct BitSlicePostingHierarchy {
    map: Mmap,
    keyspace: u64,
    rows: u32,
    bits: usize,
    words: usize,
    counts: usize,
    body: usize,
}

impl BitSlicePostingHierarchy {
    pub fn estimated_bytes(keyspace: u64, rows: u64) -> Option<u64> {
        if keyspace == 0 || rows > u32::MAX as u64 {
            return None;
        }
        let bits = bits_for_keyspace(keyspace) as u64;
        let words = rows.checked_add(63)? / 64;
        (HEADER as u64)
            .checked_add(keyspace.checked_mul(4)?)?
            .checked_add(bits.checked_mul(words)?.checked_mul(8)?)
    }

    pub fn build_from_sorted(
        sorted: impl AsRef<Path>,
        output: impl AsRef<Path>,
        keyspace: u64,
        rows: u64,
    ) -> io::Result<()> {
        let bytes = Self::estimated_bytes(keyspace, rows).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid bit-slice dimensions")
        })?;
        let rows_u32 = rows as u32;
        let bits = bits_for_keyspace(keyspace) as usize;
        let words = (rows as usize + 63) / 64;
        let counts = HEADER;
        let body = counts
            .checked_add((keyspace as usize).checked_mul(4).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "bit-slice count overflow")
            })?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bit-slice offset overflow"))?;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(output)?;
        file.set_len(bytes)?;
        let mut map = unsafe { MmapMut::map_mut(&file)? };
        map.fill(0);
        map[..8].copy_from_slice(MAGIC);
        map[8..16].copy_from_slice(&keyspace.to_le_bytes());
        map[16..20].copy_from_slice(&rows_u32.to_le_bytes());
        map[20..24].copy_from_slice(&(bits as u32).to_le_bytes());
        map[24..32].copy_from_slice(&(words as u64).to_le_bytes());
        map[32..40].copy_from_slice(&(counts as u64).to_le_bytes());
        map[40..48].copy_from_slice(&(body as u64).to_le_bytes());

        let mut counts_mem = vec![0u32; keyspace as usize];
        let mut reader = BufReader::new(File::open(sorted)?);
        let mut seen = 0u64;
        while let Some((key, row)) = read_record(&mut reader)? {
            if key >= keyspace || row as u64 >= rows {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bit-slice record outside declared dimensions",
                ));
            }
            counts_mem[key as usize] = counts_mem[key as usize]
                .checked_add(1)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bit-slice count overflow"))?;
            let word = row as usize / 64;
            let row_bit = row as usize % 64;
            for bit in 0..bits {
                if (key >> bit) & 1 == 0 {
                    continue;
                }
                let off = body + (bit * words + word) * 8;
                let mut value = u64::from_le_bytes(map[off..off + 8].try_into().unwrap());
                value |= 1u64 << row_bit;
                map[off..off + 8].copy_from_slice(&value.to_le_bytes());
            }
            seen += 1;
        }
        if seen != rows {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bit-slice source row count mismatch",
            ));
        }
        for (key, count) in counts_mem.into_iter().enumerate() {
            let off = counts + key * 4;
            map[off..off + 4].copy_from_slice(&count.to_le_bytes());
        }
        map.flush()
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER || &map[..8] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid bit-slice header",
            ));
        }
        let keyspace = u64::from_le_bytes(map[8..16].try_into().unwrap());
        let rows = u32::from_le_bytes(map[16..20].try_into().unwrap());
        let bits = u32::from_le_bytes(map[20..24].try_into().unwrap()) as usize;
        let words_u64 = u64::from_le_bytes(map[24..32].try_into().unwrap());
        let counts = u64::from_le_bytes(map[32..40].try_into().unwrap()) as usize;
        let body = u64::from_le_bytes(map[40..48].try_into().unwrap()) as usize;
        let words = (rows as usize + 63) / 64;
        let expected = Self::estimated_bytes(keyspace, rows as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid bit-slice layout"))?
            as usize;
        if keyspace == 0
            || bits != bits_for_keyspace(keyspace) as usize
            || words_u64 != words as u64
            || counts != HEADER
            || body != HEADER + keyspace as usize * 4
            || map.len() != expected
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bit-slice layout mismatch",
            ));
        }
        Ok(Self {
            map,
            keyspace,
            rows,
            bits,
            words,
            counts,
            body,
        })
    }

    fn count_at(&self, key: u64) -> usize {
        let off = self.counts + key as usize * 4;
        u32::from_le_bytes(self.map[off..off + 4].try_into().unwrap()) as usize
    }

    fn plane_word(&self, bit: usize, word: usize) -> u64 {
        let off = self.body + (bit * self.words + word) * 8;
        u64::from_le_bytes(self.map[off..off + 8].try_into().unwrap())
    }

    fn last_mask(&self, word: usize) -> u64 {
        if word + 1 != self.words || self.rows as usize % 64 == 0 {
            u64::MAX
        } else {
            (1u64 << (self.rows as usize % 64)) - 1
        }
    }

    fn equality_word(&self, key: u64, word: usize) -> u64 {
        if key >= self.keyspace || word >= self.words {
            return 0;
        }
        let mut out = u64::MAX;
        for bit in 0..self.bits {
            let plane = self.plane_word(bit, word);
            if (key >> bit) & 1 != 0 {
                out &= plane;
            } else {
                out &= !plane;
            }
        }
        out & self.last_mask(word)
    }

    fn matches_row(&self, key: u64, row: u32) -> bool {
        if key >= self.keyspace || row >= self.rows {
            return false;
        }
        let word = row as usize / 64;
        let bit = row as usize % 64;
        self.equality_word(key, word) & (1u64 << bit) != 0
    }

    pub fn row_count(&self, key: u64) -> usize {
        if key >= self.keyspace {
            0
        } else {
            self.count_at(key)
        }
    }

    pub fn rows(&self, key: u64) -> Vec<u32> {
        if key >= self.keyspace {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.count_at(key));
        for word in 0..self.words {
            let mut mask = self.equality_word(key, word);
            while mask != 0 {
                let bit = mask.trailing_zeros() as usize;
                out.push((word * 64 + bit) as u32);
                mask &= mask - 1;
            }
        }
        out
    }

    pub fn intersect_rows(&self, key: u64, seed: &[u32]) -> Vec<u32> {
        if key >= self.keyspace || seed.is_empty() {
            return Vec::new();
        }
        seed.iter()
            .copied()
            .filter(|&row| self.matches_row(key, row))
            .collect()
    }

    pub fn intersect_hierarchy(
        &self,
        key: u64,
        other: &BitSlicePostingHierarchy,
        other_key: u64,
    ) -> Vec<u32> {
        if key >= self.keyspace
            || other_key >= other.keyspace
            || self.rows != other.rows
            || self.words != other.words
        {
            return Vec::new();
        }
        let capacity = self.count_at(key).min(other.count_at(other_key));
        let mut out = Vec::with_capacity(capacity);
        for word in 0..self.words {
            let mut mask = self.equality_word(key, word) & other.equality_word(other_key, word);
            while mask != 0 {
                let bit = mask.trailing_zeros() as usize;
                out.push((word * 64 + bit) as u32);
                mask &= mask - 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn builds_and_intersects_exact_values() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("sorted");
        let output = d.path().join("bits");
        let mut f = File::create(&source).unwrap();
        for (key, row) in [(0u64, 0u32), (0, 3), (1, 1), (1, 4), (2, 2), (2, 5)] {
            f.write_all(&key.to_le_bytes()).unwrap();
            f.write_all(&row.to_le_bytes()).unwrap();
        }
        drop(f);
        BitSlicePostingHierarchy::build_from_sorted(&source, &output, 3, 6).unwrap();
        let x = BitSlicePostingHierarchy::open(&output).unwrap();
        assert_eq!(x.row_count(0), 2);
        assert_eq!(x.rows(0), vec![0, 3]);
        assert_eq!(x.rows(1), vec![1, 4]);
        assert_eq!(x.rows(2), vec![2, 5]);
        assert_eq!(x.intersect_rows(1, &[0, 1, 2, 4, 5]), vec![1, 4]);

        let source2 = d.path().join("sorted2");
        let output2 = d.path().join("bits2");
        let mut f = File::create(&source2).unwrap();
        for (key, row) in [(0u64, 0u32), (0, 1), (0, 2), (1, 3), (1, 4), (1, 5)] {
            f.write_all(&key.to_le_bytes()).unwrap();
            f.write_all(&row.to_le_bytes()).unwrap();
        }
        drop(f);
        BitSlicePostingHierarchy::build_from_sorted(&source2, &output2, 2, 6).unwrap();
        let y = BitSlicePostingHierarchy::open(&output2).unwrap();
        assert_eq!(x.intersect_hierarchy(1, &y, 0), vec![1]);
    }
}

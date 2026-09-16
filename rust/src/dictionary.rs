use memmap2::Mmap;
use std::{
    cmp::Ordering,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::Path,
};

const MAGIC: &[u8; 8] = b"LHRDCT01";
const HEADER: usize = 40;
const FLAG_NULLABLE: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodedValue<'a> {
    Null,
    Text(&'a str),
}

fn read_record<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0usize;
    while got < len.len() {
        let n = reader.read(&mut len[got..])?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated dictionary record length",
            ));
        }
        got += n;
    }
    let n = u32::from_le_bytes(len) as usize;
    let mut value = vec![0u8; n];
    reader.read_exact(&mut value)?;
    Ok(Some(value))
}

/// Write one length-prefixed UTF-8 dictionary source record.
pub fn write_dictionary_record<W: Write>(writer: &mut W, value: &str) -> io::Result<()> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "dictionary value exceeds u32 length")
    })?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(bytes)
}

pub struct Dictionary {
    map: Mmap,
    values: u64,
    nullable: bool,
    offsets: usize,
    data: usize,
}

impl Dictionary {
    /// Build from lexicographically sorted length-prefixed records. Duplicate adjacent values
    /// are removed while streaming, so memory use is bounded by the longest individual value.
    pub fn build_from_sorted_records(
        sorted_records: impl AsRef<Path>,
        output: impl AsRef<Path>,
        nullable: bool,
    ) -> io::Result<u64> {
        let sorted_records = sorted_records.as_ref();
        let output = output.as_ref();
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let pid = std::process::id();
        let offsets_tmp = output.with_extension(format!("offsets.tmp-{pid}"));
        let data_tmp = output.with_extension(format!("data.tmp-{pid}"));
        let final_tmp = output.with_extension(format!("dict.tmp-{pid}"));

        let result = (|| {
            let mut input = BufReader::new(File::open(sorted_records)?);
            let mut offsets = BufWriter::new(File::create(&offsets_tmp)?);
            let mut data = BufWriter::new(File::create(&data_tmp)?);
            offsets.write_all(&0u64.to_le_bytes())?;

            let mut count = 0u64;
            let mut data_bytes = 0u64;
            let mut previous: Option<Vec<u8>> = None;
            while let Some(value) = read_record(&mut input)? {
                std::str::from_utf8(&value)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if let Some(prev) = previous.as_ref() {
                    match value.as_slice().cmp(prev.as_slice()) {
                        Ordering::Less => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "dictionary source records are not sorted",
                            ))
                        }
                        Ordering::Equal => continue,
                        Ordering::Greater => {}
                    }
                }
                data.write_all(&value)?;
                data_bytes = data_bytes
                    .checked_add(value.len() as u64)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dictionary size overflow"))?;
                offsets.write_all(&data_bytes.to_le_bytes())?;
                count = count
                    .checked_add(1)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dictionary count overflow"))?;
                previous = Some(value);
            }
            let cardinality = count + u64::from(nullable);
            if cardinality > u32::MAX as u64 + 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "dictionary cardinality exceeds u32 token space",
                ));
            }
            offsets.flush()?;
            data.flush()?;
            offsets.get_ref().sync_all()?;
            data.get_ref().sync_all()?;

            let offsets_bytes = (count + 1)
                .checked_mul(8)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dictionary offset overflow"))?;
            let data_start = (HEADER as u64)
                .checked_add(offsets_bytes)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dictionary layout overflow"))?;

            let mut out = BufWriter::new(File::create(&final_tmp)?);
            out.write_all(MAGIC)?;
            out.write_all(&count.to_le_bytes())?;
            out.write_all(&(if nullable { FLAG_NULLABLE } else { 0 }).to_le_bytes())?;
            out.write_all(&(HEADER as u64).to_le_bytes())?;
            out.write_all(&data_start.to_le_bytes())?;
            io::copy(&mut BufReader::new(File::open(&offsets_tmp)?), &mut out)?;
            io::copy(&mut BufReader::new(File::open(&data_tmp)?), &mut out)?;
            out.flush()?;
            out.get_ref().sync_all()?;
            fs::rename(&final_tmp, output)?;
            Ok(cardinality)
        })();

        let _ = fs::remove_file(&offsets_tmp);
        let _ = fs::remove_file(&data_tmp);
        if result.is_err() {
            let _ = fs::remove_file(&final_tmp);
        }
        result
    }

    /// Convenience builder for small callers/tests. Production ingestion should externally sort
    /// source records and call `build_from_sorted_records` instead.
    pub fn build_from_values<I, S>(
        values: I,
        output: impl AsRef<Path>,
        nullable: bool,
    ) -> io::Result<u64>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let output = output.as_ref();
        let mut values: Vec<String> = values
            .into_iter()
            .map(|x| x.as_ref().to_owned())
            .collect();
        values.sort();
        values.dedup();
        let source = output.with_extension(format!("records.tmp-{}", std::process::id()));
        let result = (|| {
            let mut writer = BufWriter::new(File::create(&source)?);
            for value in &values {
                write_dictionary_record(&mut writer, value)?;
            }
            writer.flush()?;
            Dictionary::build_from_sorted_records(&source, output, nullable)
        })();
        let _ = fs::remove_file(source);
        result
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER || &map[..8] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad dictionary header"));
        }
        let values = u64::from_le_bytes(map[8..16].try_into().unwrap());
        let flags = u64::from_le_bytes(map[16..24].try_into().unwrap());
        if flags & !FLAG_NULLABLE != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown dictionary flags"));
        }
        let offsets = u64::from_le_bytes(map[24..32].try_into().unwrap()) as usize;
        let data = u64::from_le_bytes(map[32..40].try_into().unwrap()) as usize;
        let expected_data = HEADER
            .checked_add((values as usize + 1).checked_mul(8).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "dictionary layout overflow")
            })?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dictionary layout overflow"))?;
        if offsets != HEADER || data != expected_data || data > map.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad dictionary layout"));
        }
        let this = Self {
            map,
            values,
            nullable: flags & FLAG_NULLABLE != 0,
            offsets,
            data,
        };
        if this.offset_at(0)? != 0 || this.offset_at(values as usize)? as usize > this.map.len() - data {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad dictionary offsets"));
        }
        for i in 1..=values as usize {
            if this.offset_at(i)? < this.offset_at(i - 1)? {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "unsorted dictionary offsets"));
            }
        }
        Ok(this)
    }

    fn offset_at(&self, index: usize) -> io::Result<u64> {
        if index > self.values as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "dictionary offset index out of range"));
        }
        let o = self.offsets + index * 8;
        Ok(u64::from_le_bytes(self.map[o..o + 8].try_into().unwrap()))
    }

    fn text_at(&self, index: usize) -> Option<&str> {
        if index >= self.values as usize {
            return None;
        }
        let start = self.offset_at(index).ok()? as usize;
        let end = self.offset_at(index + 1).ok()? as usize;
        if start > end || self.data + end > self.map.len() {
            return None;
        }
        std::str::from_utf8(&self.map[self.data + start..self.data + end]).ok()
    }

    pub fn nullable(&self) -> bool {
        self.nullable
    }

    pub fn value_count(&self) -> u64 {
        self.values
    }

    pub fn cardinality(&self) -> u64 {
        self.values + u64::from(self.nullable)
    }

    pub fn decode(&self, token: u32) -> Option<DecodedValue<'_>> {
        if self.nullable {
            if token == 0 {
                return Some(DecodedValue::Null);
            }
            self.text_at(token as usize - 1).map(DecodedValue::Text)
        } else {
            self.text_at(token as usize).map(DecodedValue::Text)
        }
    }

    pub fn token(&self, value: &str) -> Option<u32> {
        let bytes = value.as_bytes();
        let mut lo = 0usize;
        let mut hi = self.values as usize;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let current = self.text_at(mid)?.as_bytes();
            if current < bytes {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < self.values as usize && self.text_at(lo)? == value {
            u32::try_from(lo + usize::from(self.nullable)).ok()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_nullable_dictionary() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("x.dict");
        let card = Dictionary::build_from_values(["zeta", "alpha", "alpha", "beta"], &path, true).unwrap();
        assert_eq!(card, 4);
        let dict = Dictionary::open(path).unwrap();
        assert_eq!(dict.decode(0), Some(DecodedValue::Null));
        assert_eq!(dict.token("alpha"), Some(1));
        assert_eq!(dict.token("beta"), Some(2));
        assert_eq!(dict.token("zeta"), Some(3));
        assert_eq!(dict.decode(3), Some(DecodedValue::Text("zeta")));
        assert_eq!(dict.token("missing"), None);
    }
}

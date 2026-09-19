use crate::{
    dictionary::{DecodedValue, Dictionary},
    external::external_sort,
    schema::{DatasetSchema, LogicalType},
};
use memmap2::Mmap;
use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::Path,
};

const MAGIC: &[u8; 8] = b"LHRNORD1";
const HEADER: usize = 56;
const NULL_RANK: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericKind {
    Unsigned,
    Signed,
}

impl NumericKind {
    fn code(self) -> u32 {
        match self {
            Self::Unsigned => 1,
            Self::Signed => 2,
        }
    }

    fn from_code(code: u32) -> io::Result<Self> {
        match code {
            1 => Ok(Self::Unsigned),
            2 => Ok(Self::Signed),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown numeric-order kind",
            )),
        }
    }

    pub fn from_logical_type(kind: &LogicalType) -> Option<Self> {
        match kind {
            LogicalType::Unsigned => Some(Self::Unsigned),
            LogicalType::Signed => Some(Self::Signed),
            _ => None,
        }
    }

    fn encode_text(self, value: &str) -> io::Result<u64> {
        match self {
            Self::Unsigned => value.parse::<u64>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid canonical unsigned value: {error}"),
                )
            }),
            Self::Signed => value
                .parse::<i64>()
                .map(|value| (value as u64) ^ (1u64 << 63))
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid canonical signed value: {error}"),
                    )
                }),
        }
    }

    fn encode_bound(self, value: &str) -> io::Result<u64> {
        self.encode_text(value).map_err(|error| {
            io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
        })
    }
}

pub fn numeric_order_filename(column: usize) -> String {
    format!("n{column:04}.nord")
}

fn read_record<R: Read>(reader: &mut R) -> io::Result<Option<(u64, u32)>> {
    let mut buf = [0u8; 12];
    let mut got = 0usize;
    while got < buf.len() {
        match reader.read(&mut buf[got..])? {
            0 if got == 0 => return Ok(None),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated numeric-order sort record",
                ))
            }
            n => got += n,
        }
    }
    Ok(Some((
        u64::from_le_bytes(buf[..8].try_into().unwrap()),
        u32::from_le_bytes(buf[8..].try_into().unwrap()),
    )))
}

fn write_record<W: Write>(writer: &mut W, key: u64, value: u32) -> io::Result<()> {
    writer.write_all(&key.to_le_bytes())?;
    writer.write_all(&value.to_le_bytes())
}

pub struct NumericOrder {
    map: Mmap,
    kind: NumericKind,
    tokens: u64,
    values: u64,
    nullable: bool,
    values_offset: usize,
    tokens_offset: usize,
    ranks_offset: usize,
}

impl NumericOrder {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER || &map[..8] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad numeric-order header",
            ));
        }
        let kind = NumericKind::from_code(u32::from_le_bytes(
            map[8..12].try_into().unwrap(),
        ))?;
        let flags = u32::from_le_bytes(map[12..16].try_into().unwrap());
        if flags & !1 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown numeric-order flags",
            ));
        }
        let nullable = flags & 1 != 0;
        let tokens = u64::from_le_bytes(map[16..24].try_into().unwrap());
        let values = u64::from_le_bytes(map[24..32].try_into().unwrap());
        let values_offset = u64::from_le_bytes(map[32..40].try_into().unwrap()) as usize;
        let tokens_offset = u64::from_le_bytes(map[40..48].try_into().unwrap()) as usize;
        let ranks_offset = u64::from_le_bytes(map[48..56].try_into().unwrap()) as usize;

        let values_bytes = usize::try_from(values)
            .ok()
            .and_then(|count| count.checked_mul(8))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "numeric-order values overflow"))?;
        let tokens_by_rank_bytes = usize::try_from(values)
            .ok()
            .and_then(|count| count.checked_mul(4))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "numeric-order token table overflow"))?;
        let ranks_by_token_bytes = usize::try_from(tokens)
            .ok()
            .and_then(|count| count.checked_mul(4))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "numeric-order rank table overflow"))?;
        let expected_tokens_offset = HEADER
            .checked_add(values_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "numeric-order offset overflow"))?;
        let expected_ranks_offset = expected_tokens_offset
            .checked_add(tokens_by_rank_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "numeric-order offset overflow"))?;
        let expected_len = expected_ranks_offset
            .checked_add(ranks_by_token_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "numeric-order size overflow"))?;

        if values_offset != HEADER
            || tokens_offset != expected_tokens_offset
            || ranks_offset != expected_ranks_offset
            || map.len() != expected_len
            || tokens != values.saturating_add(u64::from(nullable))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid numeric-order layout",
            ));
        }

        Ok(Self {
            map,
            kind,
            tokens,
            values,
            nullable,
            values_offset,
            tokens_offset,
            ranks_offset,
        })
    }

    pub fn kind(&self) -> NumericKind {
        self.kind
    }

    pub fn token_count(&self) -> u64 {
        self.tokens
    }

    pub fn value_count(&self) -> u64 {
        self.values
    }

    fn value_at_rank(&self, rank: u64) -> Option<u64> {
        if rank >= self.values {
            return None;
        }
        let rank = usize::try_from(rank).ok()?;
        let offset = self.values_offset.checked_add(rank.checked_mul(8)?)?;
        Some(u64::from_le_bytes(
            self.map.get(offset..offset + 8)?.try_into().ok()?,
        ))
    }

    pub fn token_at_rank(&self, rank: u64) -> Option<u32> {
        if rank >= self.values {
            return None;
        }
        let rank = usize::try_from(rank).ok()?;
        let offset = self.tokens_offset.checked_add(rank.checked_mul(4)?)?;
        Some(u32::from_le_bytes(
            self.map.get(offset..offset + 4)?.try_into().ok()?,
        ))
    }

    pub fn rank_for_token(&self, token: u32) -> Option<u32> {
        if token as u64 >= self.tokens {
            return None;
        }
        let offset = self
            .ranks_offset
            .checked_add((token as usize).checked_mul(4)?)?;
        let rank = u32::from_le_bytes(self.map.get(offset..offset + 4)?.try_into().ok()?);
        if self.nullable && token == 0 {
            None
        } else {
            Some(rank)
        }
    }

    fn lower_bound(&self, target: u64) -> u64 {
        let mut lo = 0u64;
        let mut hi = self.values;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.value_at_rank(mid).unwrap() < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn upper_bound(&self, target: u64) -> u64 {
        let mut lo = 0u64;
        let mut hi = self.values;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.value_at_rank(mid).unwrap() <= target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub fn rank_bounds(
        &self,
        gte: Option<&str>,
        lte: Option<&str>,
    ) -> io::Result<(u64, u64)> {
        let lo = match gte {
            Some(value) => self.lower_bound(self.kind.encode_bound(value)?),
            None => 0,
        };
        let hi = match lte {
            Some(value) => self.upper_bound(self.kind.encode_bound(value)?),
            None => self.values,
        };
        Ok((lo.min(hi), hi))
    }

    pub fn token_in_rank_bounds(&self, token: u32, lo: u64, hi: u64) -> bool {
        self.rank_for_token(token)
            .map(|rank| {
                let rank = rank as u64;
                rank >= lo && rank < hi
            })
            .unwrap_or(false)
    }

    pub fn validate_for(
        &self,
        dictionary: &Dictionary,
        logical_type: &LogicalType,
    ) -> io::Result<()> {
        let expected_kind = NumericKind::from_logical_type(logical_type).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "numeric-order sidecar used for non-numeric column",
            )
        })?;
        if self.kind != expected_kind
            || self.tokens != dictionary.cardinality()
            || self.values != dictionary.value_count()
            || self.nullable != dictionary.nullable()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "numeric-order sidecar does not match dictionary/schema",
            ));
        }
        Ok(())
    }

    pub fn verify(&self) -> io::Result<()> {
        let mut previous = None;
        for rank in 0..self.values {
            let value = self.value_at_rank(rank).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "numeric-order value missing")
            })?;
            if previous.is_some_and(|old| value < old) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "numeric-order values are not sorted",
                ));
            }
            previous = Some(value);
            let token = self.token_at_rank(rank).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "numeric-order token missing")
            })?;
            if self.rank_for_token(token).map(u64::from) != Some(rank) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "numeric-order inverse tables disagree",
                ));
            }
        }

        let mut mapped = 0u64;
        for token in 0..self.tokens {
            let token = u32::try_from(token).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "numeric-order token exceeds u32")
            })?;
            if self.rank_for_token(token).is_some() {
                mapped += 1;
            }
        }
        if mapped != self.values {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "numeric-order mapped-token count mismatch",
            ));
        }
        Ok(())
    }
}

pub fn build_numeric_order(
    dictionary: &Dictionary,
    kind: NumericKind,
    output: impl AsRef<Path>,
    max_sort_records: usize,
) -> io::Result<()> {
    if max_sort_records == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "max_sort_records must be > 0",
        ));
    }
    let output = output.as_ref();
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let pid = std::process::id();
    let stem = output
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .replace(|character: char| !character.is_ascii_alphanumeric(), "_");
    let work = parent.join(format!(".nord-{stem}-{pid}"));
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir_all(&work)?;

    let result = (|| {
        let raw = work.join("numeric.raw");
        let sorted = work.join("numeric.sorted");
        let rank_raw = work.join("rank.raw");
        let rank_sorted = work.join("rank.sorted");
        let values_body = work.join("values.body");
        let tokens_body = work.join("tokens.body");
        let final_tmp = work.join("final.nord");

        let mut raw_writer = BufWriter::new(File::create(&raw)?);
        let token_count = dictionary.cardinality();
        for raw_token in 0..token_count {
            let token = u32::try_from(raw_token).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "dictionary token exceeds u32")
            })?;
            match dictionary.decode(token) {
                Some(DecodedValue::Null) => {}
                Some(DecodedValue::Text(text)) => {
                    write_record(&mut raw_writer, kind.encode_text(text)?, token)?;
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "dictionary token missing while building numeric order",
                    ))
                }
            }
        }
        raw_writer.flush()?;
        drop(raw_writer);

        let sorted_count = external_sort(&raw, &sorted, max_sort_records)?;
        if sorted_count != dictionary.value_count() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "numeric-order sort changed value count",
            ));
        }

        let mut sorted_reader = BufReader::new(File::open(&sorted)?);
        let mut values_writer = BufWriter::new(File::create(&values_body)?);
        let mut tokens_writer = BufWriter::new(File::create(&tokens_body)?);
        let mut rank_writer = BufWriter::new(File::create(&rank_raw)?);
        let mut rank = 0u64;
        while let Some((encoded, token)) = read_record(&mut sorted_reader)? {
            let rank_u32 = u32::try_from(rank).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "numeric-order rank exceeds u32",
                )
            })?;
            values_writer.write_all(&encoded.to_le_bytes())?;
            tokens_writer.write_all(&token.to_le_bytes())?;
            write_record(&mut rank_writer, token as u64, rank_u32)?;
            rank += 1;
        }
        if rank != dictionary.value_count() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "numeric-order sorted stream count mismatch",
            ));
        }
        values_writer.flush()?;
        tokens_writer.flush()?;
        rank_writer.flush()?;
        drop(values_writer);
        drop(tokens_writer);
        drop(rank_writer);

        let rank_count = external_sort(&rank_raw, &rank_sorted, max_sort_records)?;
        if rank_count != dictionary.value_count() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "numeric-order inverse sort changed value count",
            ));
        }

        let values = dictionary.value_count();
        let tokens = dictionary.cardinality();
        let values_offset = HEADER as u64;
        let tokens_offset = values_offset
            .checked_add(values.checked_mul(8).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "numeric-order size overflow")
            })?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "numeric-order size overflow"))?;
        let ranks_offset = tokens_offset
            .checked_add(values.checked_mul(4).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "numeric-order size overflow")
            })?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "numeric-order size overflow"))?;

        let final_file = File::create(&final_tmp)?;
        let mut final_writer = BufWriter::new(final_file);
        final_writer.write_all(MAGIC)?;
        final_writer.write_all(&kind.code().to_le_bytes())?;
        final_writer.write_all(&(if dictionary.nullable() { 1u32 } else { 0u32 }).to_le_bytes())?;
        final_writer.write_all(&tokens.to_le_bytes())?;
        final_writer.write_all(&values.to_le_bytes())?;
        final_writer.write_all(&values_offset.to_le_bytes())?;
        final_writer.write_all(&tokens_offset.to_le_bytes())?;
        final_writer.write_all(&ranks_offset.to_le_bytes())?;
        io::copy(&mut BufReader::new(File::open(&values_body)?), &mut final_writer)?;
        io::copy(&mut BufReader::new(File::open(&tokens_body)?), &mut final_writer)?;

        let mut ranks = BufReader::new(File::open(&rank_sorted)?);
        let mut next = read_record(&mut ranks)?;
        for raw_token in 0..tokens {
            let expected_non_null = !dictionary.nullable() || raw_token != 0;
            match next {
                Some((token, rank)) if token == raw_token => {
                    if !expected_non_null {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "NULL token unexpectedly has numeric rank",
                        ));
                    }
                    final_writer.write_all(&rank.to_le_bytes())?;
                    next = read_record(&mut ranks)?;
                }
                _ if !expected_non_null => {
                    final_writer.write_all(&NULL_RANK.to_le_bytes())?;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "numeric-order inverse table is missing token",
                    ))
                }
            }
        }
        if next.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "numeric-order inverse table has extra token",
            ));
        }
        final_writer.flush()?;
        final_writer.get_ref().sync_all()?;
        drop(final_writer);

        let built = NumericOrder::open(&final_tmp)?;
        built.validate_for(
            dictionary,
            match kind {
                NumericKind::Unsigned => &LogicalType::Unsigned,
                NumericKind::Signed => &LogicalType::Signed,
            },
        )?;
        built.verify()?;
        drop(built);

        if output.exists() {
            fs::remove_file(output)?;
        }
        fs::rename(&final_tmp, output)?;
        Ok(())
    })();

    let _ = fs::remove_dir_all(&work);
    result
}

pub fn build_numeric_orders(
    root: impl AsRef<Path>,
    schema: &DatasetSchema,
    max_sort_records: usize,
) -> io::Result<usize> {
    let root = root.as_ref();
    let routing = root.join("routing");
    fs::create_dir_all(&routing)?;
    let mut built = 0usize;
    for (column, spec) in schema.columns.iter().enumerate() {
        let Some(kind) = NumericKind::from_logical_type(&spec.logical_type) else {
            continue;
        };
        let dictionary = Dictionary::open(
            root.join("dictionaries")
                .join(crate::logical::dictionary_filename(column)),
        )?;
        build_numeric_order(
            &dictionary,
            kind,
            routing.join(numeric_order_filename(column)),
            max_sort_records,
        )?;
        built += 1;
    }
    Ok(built)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::Dictionary;

    #[test]
    fn unsigned_order_is_numeric_not_lexicographic() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary_path = dir.path().join("values.dict");
        Dictionary::build_from_values(["100", "2", "10"], &dictionary_path, false).unwrap();
        let dictionary = Dictionary::open(&dictionary_path).unwrap();
        let order_path = dir.path().join("values.nord");
        build_numeric_order(&dictionary, NumericKind::Unsigned, &order_path, 2).unwrap();
        let order = NumericOrder::open(&order_path).unwrap();
        order.validate_for(&dictionary, &LogicalType::Unsigned).unwrap();
        order.verify().unwrap();

        let token_2 = dictionary.token("2").unwrap();
        let token_10 = dictionary.token("10").unwrap();
        let token_100 = dictionary.token("100").unwrap();
        let (lo, hi) = order.rank_bounds(Some("3"), Some("99")).unwrap();
        assert!(!order.token_in_rank_bounds(token_2, lo, hi));
        assert!(order.token_in_rank_bounds(token_10, lo, hi));
        assert!(!order.token_in_rank_bounds(token_100, lo, hi));
        assert_eq!(order.token_at_rank(0), Some(token_2));
        assert_eq!(order.token_at_rank(1), Some(token_10));
        assert_eq!(order.token_at_rank(2), Some(token_100));
    }

    #[test]
    fn signed_order_handles_negative_values_and_null() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary_path = dir.path().join("values.dict");
        Dictionary::build_from_values(["-10", "7", "0", "-2"], &dictionary_path, true).unwrap();
        let dictionary = Dictionary::open(&dictionary_path).unwrap();
        let order_path = dir.path().join("values.nord");
        build_numeric_order(&dictionary, NumericKind::Signed, &order_path, 2).unwrap();
        let order = NumericOrder::open(&order_path).unwrap();
        order.validate_for(&dictionary, &LogicalType::Signed).unwrap();
        order.verify().unwrap();

        let (lo, hi) = order.rank_bounds(Some("-2"), Some("7")).unwrap();
        assert!(!order.token_in_rank_bounds(0, lo, hi));
        assert!(!order.token_in_rank_bounds(dictionary.token("-10").unwrap(), lo, hi));
        assert!(order.token_in_rank_bounds(dictionary.token("-2").unwrap(), lo, hi));
        assert!(order.token_in_rank_bounds(dictionary.token("0").unwrap(), lo, hi));
        assert!(order.token_in_rank_bounds(dictionary.token("7").unwrap(), lo, hi));
    }
}

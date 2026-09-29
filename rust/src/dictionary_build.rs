use crate::{dictionary_filename, write_dictionary_record, DatasetSchema, Dictionary};
use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

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
                "truncated dictionary sort record",
            ));
        }
        got += n;
    }
    let n = u32::from_le_bytes(len) as usize;
    let mut value = vec![0u8; n];
    reader.read_exact(&mut value)?;
    Ok(Some(value))
}

fn write_record_bytes<W: Write>(writer: &mut W, value: &[u8]) -> io::Result<()> {
    let len = u32::try_from(value.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "dictionary value exceeds u32 length")
    })?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(value)
}

fn flush_run(
    records: &mut Vec<Vec<u8>>,
    run_dir: &Path,
    run_number: usize,
) -> io::Result<PathBuf> {
    records.sort_unstable();
    records.dedup();
    let path = run_dir.join(format!("run-{run_number:06}.bin"));
    let mut writer = BufWriter::new(File::create(&path)?);
    for record in records.iter() {
        write_record_bytes(&mut writer, record)?;
    }
    writer.flush()?;
    records.clear();
    Ok(path)
}

#[derive(Eq)]
struct HeapItem {
    value: Vec<u8>,
    run: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value && self.run == other.run
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .value
            .cmp(&self.value)
            .then_with(|| other.run.cmp(&self.run))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn external_sort_dictionary(
    input: &Path,
    output: &Path,
    run_dir: &Path,
    max_run_bytes: usize,
) -> io::Result<()> {
    if max_run_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dictionary_run_bytes must be > 0",
        ));
    }
    fs::create_dir_all(run_dir)?;
    let mut reader = BufReader::new(File::open(input)?);
    let mut records = Vec::<Vec<u8>>::new();
    let mut bytes = 0usize;
    let mut runs = Vec::new();
    while let Some(record) = read_record(&mut reader)? {
        bytes = bytes.saturating_add(record.len() + 4);
        records.push(record);
        if bytes >= max_run_bytes {
            runs.push(flush_run(&mut records, run_dir, runs.len())?);
            bytes = 0;
        }
    }
    if !records.is_empty() {
        runs.push(flush_run(&mut records, run_dir, runs.len())?);
    }

    let mut output_writer = BufWriter::new(File::create(output)?);
    if runs.is_empty() {
        output_writer.flush()?;
        return Ok(());
    }

    let mut readers = runs
        .iter()
        .map(File::open)
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .map(BufReader::new)
        .collect::<Vec<_>>();
    let mut heap = BinaryHeap::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(value) = read_record(reader)? {
            heap.push(HeapItem { value, run });
        }
    }
    let mut previous: Option<Vec<u8>> = None;
    while let Some(item) = heap.pop() {
        if previous.as_deref() != Some(item.value.as_slice()) {
            write_record_bytes(&mut output_writer, &item.value)?;
            previous = Some(item.value.clone());
        }
        if let Some(value) = read_record(&mut readers[item.run])? {
            heap.push(HeapItem {
                value,
                run: item.run,
            });
        }
    }
    output_writer.flush()?;
    drop(output_writer);
    for run in runs {
        let _ = fs::remove_file(run);
    }
    let _ = fs::remove_dir(run_dir);
    Ok(())
}

/// Bounded on-disk dictionary builder shared by direct Parquet import and compaction.
/// Values passed to `push` must already be in the column's canonical string representation.
pub(crate) struct DictionarySpool {
    temp: PathBuf,
    dictionaries: PathBuf,
    raw_paths: Vec<PathBuf>,
    writers: Vec<BufWriter<File>>,
}

impl DictionarySpool {
    pub(crate) fn create(root: &Path, columns: usize) -> io::Result<Self> {
        if columns == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "schema has no columns"));
        }
        let temp = root.join("temp").join("dictionaries");
        let dictionaries = root.join("dictionaries");
        fs::create_dir_all(&temp)?;
        fs::create_dir_all(&dictionaries)?;
        let raw_paths = (0..columns)
            .map(|column| temp.join(format!("c{column:04}.raw")))
            .collect::<Vec<_>>();
        let writers = raw_paths
            .iter()
            .map(File::create)
            .collect::<io::Result<Vec<_>>>()?
            .into_iter()
            .map(BufWriter::new)
            .collect();
        Ok(Self {
            temp,
            dictionaries,
            raw_paths,
            writers,
        })
    }

    pub(crate) fn push(&mut self, column: usize, canonical: &str) -> io::Result<()> {
        let writer = self.writers.get_mut(column).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "dictionary column is out of range")
        })?;
        write_dictionary_record(writer, canonical)
    }

    pub(crate) fn finish(
        mut self,
        schema: &DatasetSchema,
        max_run_bytes: usize,
    ) -> io::Result<Vec<u64>> {
        if schema.columns.len() != self.writers.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dictionary spool/schema column count mismatch",
            ));
        }
        for writer in &mut self.writers {
            writer.flush()?;
        }
        drop(self.writers);

        let mut cardinalities = Vec::with_capacity(schema.columns.len());
        for (column_index, column) in schema.columns.iter().enumerate() {
            let sorted = self.temp.join(format!("c{column_index:04}.sorted"));
            let run_dir = self.temp.join(format!("runs-c{column_index:04}"));
            external_sort_dictionary(
                &self.raw_paths[column_index],
                &sorted,
                &run_dir,
                max_run_bytes,
            )?;
            let output = self.dictionaries.join(dictionary_filename(column_index));
            let cardinality =
                Dictionary::build_from_sorted_records(&sorted, &output, column.nullable)?;
            if cardinality == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("column {} has zero cardinality", column.name),
                ));
            }
            cardinalities.push(cardinality);
            let _ = fs::remove_file(&self.raw_paths[column_index]);
            let _ = fs::remove_file(sorted);
        }
        Ok(cardinalities)
    }
}

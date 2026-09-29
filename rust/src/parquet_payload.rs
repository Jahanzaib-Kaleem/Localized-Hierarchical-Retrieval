use arrow_array::{Array, ArrayRef, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use parquet::{
    arrow::{
        arrow_reader::ParquetRecordBatchReaderBuilder, ArrowWriter, ProjectionMask,
    },
    basic::Compression,
    file::properties::WriterProperties,
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn external(context: &str, error: impl std::fmt::Display) -> io::Error {
    invalid(format!("{context}: {error}"))
}

fn token_field_name(logical_column: usize) -> String {
    format!("c{logical_column:04}")
}

/// Bounded writer for cold canonical token columns.
///
/// The payload stores LHR dictionary token IDs, not external strings. This preserves the existing
/// dictionary/index semantics while allowing Parquet to compress repetitive token streams.
pub struct ParquetTokenWriter {
    path: PathBuf,
    columns: Vec<usize>,
    schema: SchemaRef,
    writer: ArrowWriter<File>,
    rows: u64,
}

impl ParquetTokenWriter {
    pub fn create(
        path: impl AsRef<Path>,
        columns: &[usize],
        row_group_rows: usize,
    ) -> io::Result<Self> {
        if columns.is_empty() {
            return Err(invalid("Parquet token payload requires at least one column"));
        }
        if row_group_rows == 0 {
            return Err(invalid("Parquet row_group_rows must be > 0"));
        }
        let mut ordered = columns.to_vec();
        ordered.sort_unstable();
        ordered.dedup();
        if ordered.len() != columns.len() || ordered != columns {
            return Err(invalid(
                "Parquet token payload columns must be unique and sorted",
            ));
        }

        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let fields = columns
            .iter()
            .map(|&column| Field::new(token_field_name(column), DataType::UInt32, false))
            .collect::<Vec<_>>();
        let schema = Arc::new(Schema::new(fields));
        let properties = WriterProperties::builder()
            // Snappy is deliberately the first codec: fast decode is more important than chasing
            // maximum compression. Storage/latency comparisons can tune this later.
            .set_compression(Compression::SNAPPY)
            .set_dictionary_enabled(true)
            .set_max_row_group_row_count(Some(row_group_rows))
            .build();
        let writer = ArrowWriter::try_new(File::create(&path)?, Arc::clone(&schema), Some(properties))
            .map_err(|error| external("create Parquet token writer", error))?;
        Ok(Self {
            path,
            columns: columns.to_vec(),
            schema,
            writer,
            rows: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn columns(&self) -> &[usize] {
        &self.columns
    }

    pub fn rows_written(&self) -> u64 {
        self.rows
    }

    /// Append one bounded row batch represented column-major in the same order as `columns()`.
    pub fn write_columns(&mut self, columns: &[Vec<u32>]) -> io::Result<()> {
        if columns.len() != self.columns.len() {
            return Err(invalid(format!(
                "Parquet token batch has {} columns, expected {}",
                columns.len(),
                self.columns.len()
            )));
        }
        let rows = columns.first().map_or(0, Vec::len);
        if rows == 0 {
            return Ok(());
        }
        if columns.iter().any(|column| column.len() != rows) {
            return Err(invalid("Parquet token batch columns have different row counts"));
        }
        let arrays = columns
            .iter()
            .map(|values| Arc::new(UInt32Array::from(values.clone())) as ArrayRef)
            .collect::<Vec<_>>();
        let batch = RecordBatch::try_new(Arc::clone(&self.schema), arrays)
            .map_err(|error| external("build Parquet token record batch", error))?;
        self.writer
            .write(&batch)
            .map_err(|error| external("write Parquet token batch", error))?;
        self.rows = self
            .rows
            .checked_add(rows as u64)
            .ok_or_else(|| invalid("Parquet token row count overflow"))?;
        Ok(())
    }

    pub fn finish(self) -> io::Result<u64> {
        let rows = self.rows;
        self.writer
            .close()
            .map_err(|error| external("finalize Parquet token payload", error))?;
        Ok(rows)
    }
}

/// Read selected logical columns for selected physical rows from one token payload file.
///
/// The exact-index planner should call this only after it has resolved row IDs. Requested rows are
/// grouped by Parquet row group, and each row group is decoded at most once per call. Only selected
/// columns are projected.
pub fn read_token_projection(
    path: impl AsRef<Path>,
    physical_rows: &[u64],
    logical_columns: &[usize],
) -> io::Result<Vec<Vec<u32>>> {
    if physical_rows.is_empty() {
        return Ok(Vec::new());
    }
    if logical_columns.is_empty() {
        return Ok(vec![Vec::new(); physical_rows.len()]);
    }

    let path = path.as_ref();
    let metadata_builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)
        .map_err(|error| external("open Parquet token payload", error))?;
    let arrow_schema = metadata_builder.schema().clone();
    let metadata = metadata_builder.metadata();
    let total_rows = u64::try_from(metadata.file_metadata().num_rows())
        .map_err(|_| invalid("Parquet row count is negative"))?;

    let mut group_starts = Vec::with_capacity(metadata.num_row_groups());
    let mut cursor = 0u64;
    for group in 0..metadata.num_row_groups() {
        group_starts.push(cursor);
        let group_rows = u64::try_from(metadata.row_group(group).num_rows())
            .map_err(|_| invalid("Parquet row-group row count is negative"))?;
        cursor = cursor
            .checked_add(group_rows)
            .ok_or_else(|| invalid("Parquet row count overflow"))?;
    }
    if cursor != total_rows {
        return Err(invalid(format!(
            "Parquet metadata row groups cover {cursor} rows, file reports {total_rows}"
        )));
    }

    let mut requested_positions = Vec::with_capacity(logical_columns.len());
    for &logical in logical_columns {
        let name = token_field_name(logical);
        let position = arrow_schema
            .fields()
            .iter()
            .position(|field| field.name() == &name)
            .ok_or_else(|| invalid(format!("Parquet token payload is missing column {name}")))?;
        requested_positions.push(position);
    }
    let mut projected_positions = requested_positions.clone();
    projected_positions.sort_unstable();
    projected_positions.dedup();

    let mut grouped = BTreeMap::<usize, Vec<(usize, u64)>>::new();
    for (output_index, &row) in physical_rows.iter().enumerate() {
        if row >= total_rows {
            return Err(invalid(format!(
                "physical row {row} is outside Parquet payload with {total_rows} rows"
            )));
        }
        let group = group_starts.partition_point(|&start| start <= row).saturating_sub(1);
        let local = row - group_starts[group];
        grouped.entry(group).or_default().push((output_index, local));
    }
    for targets in grouped.values_mut() {
        targets.sort_unstable_by_key(|(_, local)| *local);
    }

    let mut output = vec![None::<Vec<u32>>; physical_rows.len()];
    for (group, targets) in grouped {
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)
            .map_err(|error| external("reopen Parquet token payload", error))?;
        let schema_descr = builder.metadata().file_metadata().schema_descr();
        let mask = ProjectionMask::roots(schema_descr, projected_positions.iter().copied());
        let mut reader = builder
            .with_row_groups(vec![group])
            .with_projection(mask)
            .with_batch_size(8192)
            .build()
            .map_err(|error| external("build Parquet token projection reader", error))?;

        let mut consumed = 0u64;
        let mut target = 0usize;
        while let Some(batch) = reader.next() {
            let batch = batch.map_err(|error| external("read Parquet token batch", error))?;
            let batch_start = consumed;
            let batch_end = consumed
                .checked_add(batch.num_rows() as u64)
                .ok_or_else(|| invalid("Parquet batch row range overflow"))?;
            while target < targets.len() && targets[target].1 < batch_end {
                let (output_index, local_row) = targets[target];
                if local_row >= batch_start {
                    let offset = usize::try_from(local_row - batch_start)
                        .map_err(|_| invalid("Parquet row offset exceeds usize"))?;
                    let mut values = Vec::with_capacity(logical_columns.len());
                    for &position in &requested_positions {
                        let projected_slot = projected_positions
                            .binary_search(&position)
                            .map_err(|_| invalid("Parquet projection mapping diverged"))?;
                        let array = batch
                            .column(projected_slot)
                            .as_any()
                            .downcast_ref::<UInt32Array>()
                            .ok_or_else(|| invalid("Parquet token column is not UInt32"))?;
                        if array.is_null(offset) {
                            return Err(invalid("Parquet token payload contains a null token"));
                        }
                        values.push(array.value(offset));
                    }
                    output[output_index] = Some(values);
                }
                target += 1;
            }
            consumed = batch_end;
        }
        if target != targets.len() {
            return Err(invalid(format!(
                "Parquet row group {group} ended before requested rows were materialized"
            )));
        }
    }

    output
        .into_iter()
        .enumerate()
        .map(|(index, values)| {
            values.ok_or_else(|| {
                invalid(format!(
                    "Parquet payload did not return requested physical row {}",
                    physical_rows[index]
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_payload_projects_scattered_rows_and_reorders_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cold.parquet");
        let mut writer = ParquetTokenWriter::create(&path, &[2, 7], 4).unwrap();
        writer
            .write_columns(&[
                (0..10).map(|row| 100 + row).collect(),
                (0..10).map(|row| 200 + row).collect(),
            ])
            .unwrap();
        assert_eq!(writer.finish().unwrap(), 10);

        let rows = read_token_projection(&path, &[9, 0, 5], &[7, 2]).unwrap();
        assert_eq!(rows, vec![vec![209, 109], vec![200, 100], vec![205, 105]]);
    }

    #[test]
    fn token_payload_rejects_out_of_range_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cold.parquet");
        let mut writer = ParquetTokenWriter::create(&path, &[1], 4).unwrap();
        writer.write_columns(&[vec![1, 2, 3]]).unwrap();
        writer.finish().unwrap();
        assert!(read_token_projection(&path, &[3], &[1]).is_err());
    }
}

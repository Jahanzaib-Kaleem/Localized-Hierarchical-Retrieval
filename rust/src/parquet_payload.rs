use parquet::{
    basic::Compression,
    column::reader::get_typed_column_reader,
    data_type::Int32Type,
    file::{
        properties::WriterProperties,
        reader::{FileReader, SerializedFileReader},
        writer::SerializedFileWriter,
    },
    schema::parser::parse_message_type,
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

fn parquet_schema(columns: &[usize]) -> io::Result<Arc<parquet::schema::types::Type>> {
    let mut message = String::from("message lhr_tokens {\n");
    for &column in columns {
        message.push_str(&format!(
            "  REQUIRED INT32 {} (INTEGER(32,false));\n",
            token_field_name(column)
        ));
    }
    message.push_str("}\n");
    parse_message_type(&message)
        .map(Arc::new)
        .map_err(|error| external("build Parquet token schema", error))
}

/// Bounded writer for cold canonical token columns.
///
/// The payload stores LHR dictionary token IDs, not external strings. The low-level Parquet API is
/// used deliberately: LHR only needs required u32 token columns and should not pull the Arrow bridge
/// into its constrained-memory runtime or benchmark build.
pub struct ParquetTokenWriter {
    path: PathBuf,
    columns: Vec<usize>,
    writer: SerializedFileWriter<File>,
    pending: Vec<Vec<u32>>,
    row_group_rows: usize,
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
        let schema = parquet_schema(columns)?;
        let properties = Arc::new(
            WriterProperties::builder()
                .set_compression(Compression::SNAPPY)
                .set_dictionary_enabled(true)
                .build(),
        );
        let writer = SerializedFileWriter::new(File::create(&path)?, schema, properties)
            .map_err(|error| external("create Parquet token writer", error))?;
        Ok(Self {
            path,
            columns: columns.to_vec(),
            writer,
            pending: vec![Vec::with_capacity(row_group_rows); columns.len()],
            row_group_rows,
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

    fn flush_row_group(&mut self) -> io::Result<()> {
        let rows = self.pending.first().map_or(0, Vec::len);
        if rows == 0 {
            return Ok(());
        }
        if self.pending.iter().any(|column| column.len() != rows) {
            return Err(invalid("Parquet token row-group columns have different lengths"));
        }

        let mut row_group = self
            .writer
            .next_row_group()
            .map_err(|error| external("open Parquet token row group", error))?;
        for values in &self.pending {
            let mut column = row_group
                .next_column()
                .map_err(|error| external("open Parquet token column", error))?
                .ok_or_else(|| invalid("Parquet token schema has fewer columns than expected"))?;
            let signed = values.iter().map(|&value| value as i32).collect::<Vec<_>>();
            let written = column
                .typed::<Int32Type>()
                .write_batch(&signed, None, None)
                .map_err(|error| external("write Parquet token column", error))?;
            if written != rows {
                return Err(invalid(format!(
                    "Parquet token column wrote {written} values, expected {rows}"
                )));
            }
            column
                .close()
                .map_err(|error| external("close Parquet token column", error))?;
        }
        if row_group
            .next_column()
            .map_err(|error| external("check Parquet token column count", error))?
            .is_some()
        {
            return Err(invalid("Parquet token schema has more columns than expected"));
        }
        row_group
            .close()
            .map_err(|error| external("close Parquet token row group", error))?;
        for column in &mut self.pending {
            column.clear();
        }
        Ok(())
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

        let mut offset = 0usize;
        while offset < rows {
            let pending_rows = self.pending[0].len();
            let available = self.row_group_rows - pending_rows;
            let take = available.min(rows - offset);
            for (pending, input) in self.pending.iter_mut().zip(columns) {
                pending.extend_from_slice(&input[offset..offset + take]);
            }
            offset += take;
            if self.pending[0].len() == self.row_group_rows {
                self.flush_row_group()?;
            }
        }
        self.rows = self
            .rows
            .checked_add(rows as u64)
            .ok_or_else(|| invalid("Parquet token row count overflow"))?;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<u64> {
        self.flush_row_group()?;
        let rows = self.rows;
        self.writer
            .close()
            .map_err(|error| external("finalize Parquet token payload", error))?;
        Ok(rows)
    }
}

/// Read selected logical columns for selected physical rows from one token payload file.
///
/// Requested rows are grouped by Parquet row group. Each selected column chunk is decoded once for
/// that group, and only the requested row offsets are copied into the result.
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

    let reader = SerializedFileReader::new(File::open(path.as_ref())?)
        .map_err(|error| external("open Parquet token payload", error))?;
    let metadata = reader.metadata();
    let total_rows = u64::try_from(metadata.file_metadata().num_rows())
        .map_err(|_| invalid("Parquet row count is negative"))?;
    let schema = metadata.file_metadata().schema_descr();

    let mut requested_positions = Vec::with_capacity(logical_columns.len());
    for &logical in logical_columns {
        let name = token_field_name(logical);
        let position = (0..schema.num_columns())
            .find(|&index| schema.column(index).name() == name)
            .ok_or_else(|| invalid(format!("Parquet token payload is missing column {name}")))?;
        requested_positions.push(position);
    }

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

    let mut grouped = BTreeMap::<usize, Vec<(usize, usize)>>::new();
    for (output_index, &row) in physical_rows.iter().enumerate() {
        if row >= total_rows {
            return Err(invalid(format!(
                "physical row {row} is outside Parquet payload with {total_rows} rows"
            )));
        }
        let group = group_starts
            .partition_point(|&start| start <= row)
            .saturating_sub(1);
        let local = usize::try_from(row - group_starts[group])
            .map_err(|_| invalid("Parquet row offset exceeds usize"))?;
        grouped.entry(group).or_default().push((output_index, local));
    }

    let mut output = vec![vec![0u32; logical_columns.len()]; physical_rows.len()];
    for (group, targets) in grouped {
        let row_group = reader
            .get_row_group(group)
            .map_err(|error| external("open Parquet token row group", error))?;
        let group_rows = usize::try_from(row_group.metadata().num_rows())
            .map_err(|_| invalid("Parquet row-group row count exceeds usize"))?;
        for (request_slot, &column_position) in requested_positions.iter().enumerate() {
            let column = row_group
                .get_column_reader(column_position)
                .map_err(|error| external("open Parquet token column reader", error))?;
            let mut column = get_typed_column_reader::<Int32Type>(column);
            let mut values = Vec::<i32>::with_capacity(group_rows);
            let (records, values_read, _) = column
                .read_records(group_rows, None, None, &mut values)
                .map_err(|error| external("read Parquet token column", error))?;
            if records != group_rows || values_read != group_rows || values.len() != group_rows {
                return Err(invalid(format!(
                    "Parquet token column returned records={records} values={values_read} len={}, expected {group_rows}",
                    values.len()
                )));
            }
            for &(output_index, local) in &targets {
                output[output_index][request_slot] = values[local] as u32;
            }
        }
    }
    Ok(output)
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
    fn token_payload_roundtrips_full_u32_domain_bits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cold.parquet");
        let mut writer = ParquetTokenWriter::create(&path, &[1], 2).unwrap();
        writer
            .write_columns(&[vec![0, i32::MAX as u32, i32::MAX as u32 + 1, u32::MAX]])
            .unwrap();
        writer.finish().unwrap();
        let rows = read_token_projection(&path, &[0, 1, 2, 3], &[1]).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![0],
                vec![i32::MAX as u32],
                vec![i32::MAX as u32 + 1],
                vec![u32::MAX]
            ]
        );
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

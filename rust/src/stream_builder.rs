use crate::{
    write_storage_layout, BuildConfig, Manifest, ParquetPayloadFile, ParquetTokenWriter, Segment,
    SegmentMeta, StorageLayout,
};
use std::{fs, io, path::{Path, PathBuf}};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn validate_config(cfg: &BuildConfig) -> io::Result<()> {
    if cfg.columns == 0
        || cfg.page_rows == 0
        || cfg.cardinalities.len() != cfg.columns
        || cfg.max_sort_records == 0
    {
        return Err(invalid("invalid stream build config"));
    }
    if cfg.cardinalities.iter().any(|&value| value == 0 || value > u32::MAX as u64 + 1) {
        return Err(invalid("cardinality cannot be represented by u32 tokens"));
    }
    if !cfg.hierarchies.is_empty() {
        return Err(invalid(
            "stream builder expects hierarchies to be added after canonical storage is complete",
        ));
    }
    Ok(())
}

fn partition(columns: usize, cold: &[usize]) -> io::Result<Vec<usize>> {
    if cold.is_empty() {
        return Err(invalid("hybrid stream storage requires at least one cold column"));
    }
    let mut ordered = cold.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    if ordered != cold || ordered.iter().any(|&column| column >= columns) {
        return Err(invalid(
            "hybrid cold columns must be sorted, unique, and in range",
        ));
    }
    let hot = (0..columns)
        .filter(|column| ordered.binary_search(column).is_err())
        .collect::<Vec<_>>();
    if hot.is_empty() {
        return Err(invalid("hybrid stream storage requires at least one hot column"));
    }
    Ok(hot)
}

/// Incremental u32 canonical writer used by large direct imports and compaction.
///
/// It deliberately builds no routing indexes while rows are being streamed. Once `finish` writes
/// the immutable canonical representation, callers add exact hierarchies through the normal LHR
/// index builder. This keeps peak memory bounded to roughly one caller batch plus one page-aligned
/// carry buffer and one cold Parquet row group.
pub(crate) struct U32StreamBuilder {
    root: PathBuf,
    cfg: BuildConfig,
    cold_columns: Option<Vec<usize>>,
    hot_columns: Vec<usize>,
    row_group_rows: usize,
    carry: Vec<u32>,
    segments: Vec<SegmentMeta>,
    payloads: Vec<ParquetPayloadFile>,
    page_id: u32,
    row_start: u64,
    segment_number: usize,
}

impl U32StreamBuilder {
    pub(crate) fn create(
        root: impl AsRef<Path>,
        cfg: BuildConfig,
        cold_columns: Option<Vec<usize>>,
        row_group_rows: usize,
    ) -> io::Result<Self> {
        validate_config(&cfg)?;
        let hot_columns = match cold_columns.as_deref() {
            Some(cold) => {
                if row_group_rows == 0 {
                    return Err(invalid("Parquet row_group_rows must be > 0"));
                }
                partition(cfg.columns, cold)?
            }
            None => (0..cfg.columns).collect(),
        };
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("canonical"))?;
        fs::create_dir_all(root.join("routing"))?;
        fs::create_dir_all(root.join("temp"))?;
        let page_tokens = cfg
            .page_rows
            .checked_mul(cfg.columns)
            .ok_or_else(|| invalid("page token count overflow"))?;
        Ok(Self {
            root,
            cfg,
            cold_columns,
            hot_columns,
            row_group_rows,
            carry: Vec::with_capacity(page_tokens.saturating_mul(2)),
            segments: Vec::new(),
            payloads: Vec::new(),
            page_id: 0,
            row_start: 0,
            segment_number: 0,
        })
    }

    pub(crate) fn rows_written(&self) -> u64 {
        self.row_start + (self.carry.len() / self.cfg.columns) as u64
    }

    fn emit_segment(&mut self, data: &[u32], final_partial: bool) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if data.len() % self.cfg.columns != 0 {
            return Err(invalid("token batch is not whole rows"));
        }
        let rows = data.len() / self.cfg.columns;
        if !final_partial && rows % self.cfg.page_rows != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "non-final streamed segment is not page aligned",
            ));
        }
        let first_page = self.page_id;
        let pages = if rows == 0 {
            0
        } else {
            (rows + self.cfg.page_rows - 1) / self.cfg.page_rows
        };
        self.page_id = self
            .page_id
            .checked_add(u32::try_from(pages).map_err(|_| invalid("page count exceeds u32"))?)
            .ok_or_else(|| invalid("page id overflow"))?;

        let segment_name = format!("segment-{:06}.lhr", self.segment_number);
        let canonical = self.root.join("canonical");
        if let Some(cold) = self.cold_columns.as_deref() {
            let parquet_name = format!("segment-{:06}.parquet", self.segment_number);
            let mut hot_data = Vec::with_capacity(rows * self.hot_columns.len() * 4);
            let mut cold_data = vec![Vec::<u32>::with_capacity(rows); cold.len()];
            for row in 0..rows {
                let base = row * self.cfg.columns;
                for &column in &self.hot_columns {
                    hot_data.extend_from_slice(&data[base + column].to_le_bytes());
                }
                for (slot, &column) in cold.iter().enumerate() {
                    cold_data[slot].push(data[base + column]);
                }
            }
            let mut writer = ParquetTokenWriter::create(
                canonical.join(&parquet_name),
                cold,
                self.row_group_rows,
            )?;
            writer.write_columns(&cold_data)?;
            if writer.finish()? != rows as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "hybrid Parquet sidecar row count mismatch",
                ));
            }
            Segment::write_hybrid(
                canonical.join(&segment_name),
                rows as u64,
                self.cfg.columns as u32,
                4,
                &self.hot_columns,
                cold,
                self.row_group_rows,
                &parquet_name,
                &hot_data,
            )?;
            self.payloads.push(ParquetPayloadFile {
                file: format!("canonical/{parquet_name}"),
                row_start: self.row_start,
                rows: rows as u64,
            });
        } else {
            let mut bytes = Vec::with_capacity(data.len() * 4);
            for &token in data {
                bytes.extend_from_slice(&token.to_le_bytes());
            }
            Segment::write(
                canonical.join(&segment_name),
                rows as u64,
                self.cfg.columns as u32,
                4,
                &bytes,
            )?;
        }

        self.segments.push(SegmentMeta {
            file: segment_name,
            row_start: self.row_start,
            rows: rows as u64,
            first_page,
        });
        self.row_start = self
            .row_start
            .checked_add(rows as u64)
            .ok_or_else(|| invalid("row count overflow"))?;
        self.segment_number += 1;
        Ok(())
    }

    pub(crate) fn push_batch(&mut self, batch: Vec<u32>) -> io::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        if batch.len() % self.cfg.columns != 0 {
            return Err(invalid("token batch is not whole rows"));
        }
        self.carry.extend(batch);
        let page_tokens = self
            .cfg
            .page_rows
            .checked_mul(self.cfg.columns)
            .ok_or_else(|| invalid("page token count overflow"))?;
        let full = self.carry.len() / page_tokens * page_tokens;
        if full > 0 {
            let tail = self.carry.split_off(full);
            let ready = std::mem::replace(&mut self.carry, tail);
            self.emit_segment(&ready, false)?;
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<Manifest> {
        if !self.carry.is_empty() {
            let ready = std::mem::take(&mut self.carry);
            self.emit_segment(&ready, true)?;
        }
        if self.row_start == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot build an empty LHR dataset",
            ));
        }
        let manifest = Manifest {
            format: "LHR/1".into(),
            rows: self.row_start,
            columns: self.cfg.columns,
            page_rows: self.cfg.page_rows,
            pages: self.page_id,
            cardinalities: self.cfg.cardinalities.clone(),
            segments: self.segments,
            hierarchies: Vec::new(),
        };
        let tmp = self.root.join("manifest.json.tmp");
        fs::write(
            &tmp,
            serde_json::to_vec_pretty(&manifest)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        )?;
        fs::rename(tmp, self.root.join("manifest.json"))?;

        if let Some(cold) = self.cold_columns {
            let mut layout = StorageLayout::hybrid(self.hot_columns, cold, self.payloads);
            layout.row_group_rows = self.row_group_rows;
            layout.validate(self.cfg.columns, self.row_start)?;
            write_storage_layout(&self.root, &layout)?;
        }
        Ok(manifest)
    }
}

use crate::{
    bitmap::BitmapHierarchy,
    external::external_sort,
    manifest::{HierarchyMeta, Manifest, SegmentMeta},
    write_storage_layout, ParquetPayloadFile, ParquetTokenWriter, Segment, StorageLayout,
};
use std::{
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

const MAX_AUTO_BITMAP_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct HierarchySpec {
    pub columns: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct BuildConfig {
    pub columns: usize,
    pub page_rows: usize,
    pub cardinalities: Vec<u64>,
    pub hierarchies: Vec<HierarchySpec>,
    pub max_sort_records: usize,
}

fn write_record<W: Write>(w: &mut W, key: u64, page: u32) -> io::Result<()> {
    w.write_all(&key.to_le_bytes())?;
    w.write_all(&page.to_le_bytes())
}

fn keyspace(spec: &HierarchySpec, card: &[u64]) -> io::Result<u64> {
    spec.columns.iter().try_fold(1u64, |acc, &c| {
        acc.checked_mul(card[c]).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "hierarchy keyspace overflow")
        })
    })
}

fn read_token(data: &[u8], token_index: usize, width: usize) -> u64 {
    let offset = token_index * width;
    match width {
        1 => data[offset] as u64,
        2 => u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as u64,
        4 => u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as u64,
        8 => u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()),
        _ => unreachable!(),
    }
}

fn page_keys(
    page: &[u8],
    columns: usize,
    width: usize,
    spec: &HierarchySpec,
    card: &[u64],
) -> io::Result<Vec<u64>> {
    let row_bytes = columns * width;
    let rows = page.len() / row_bytes;
    let mut keys = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut key = 0u64;
        for &column in &spec.columns {
            let value = read_token(page, row * columns + column, width);
            let radix = card[column];
            if value >= radix {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "token outside declared cardinality",
                ));
            }
            key = key
                .checked_mul(radix)
                .and_then(|value_so_far| value_so_far.checked_add(value))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "hierarchy key overflow")
                })?;
        }
        keys.push(key);
    }
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

fn validate_config(cfg: &BuildConfig, max_cardinality: u64) -> io::Result<()> {
    if cfg.columns == 0
        || cfg.page_rows == 0
        || cfg.cardinalities.len() != cfg.columns
        || cfg.max_sort_records == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid build config",
        ));
    }
    if cfg
        .cardinalities
        .iter()
        .any(|&value| value == 0 || value > max_cardinality)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cardinality cannot be represented by selected token width",
        ));
    }
    if cfg
        .hierarchies
        .iter()
        .any(|hierarchy| {
            hierarchy.columns.is_empty()
                || hierarchy.columns.iter().any(|&column| column >= cfg.columns)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid hierarchy columns",
        ));
    }
    Ok(())
}

fn hybrid_partition(columns: usize, cold_columns: &[usize]) -> io::Result<Vec<usize>> {
    if cold_columns.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hybrid storage requires at least one cold column",
        ));
    }
    let mut ordered = cold_columns.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    if ordered != cold_columns || ordered.iter().any(|&column| column >= columns) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hybrid cold columns must be sorted, unique, and in range",
        ));
    }
    let hot = (0..columns)
        .filter(|column| ordered.binary_search(column).is_err())
        .collect::<Vec<_>>();
    if hot.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hybrid storage requires at least one hot column",
        ));
    }
    Ok(hot)
}

fn build_encoded_batches<I>(
    batches: I,
    root: impl AsRef<Path>,
    cfg: &BuildConfig,
    width: usize,
    max_cardinality: u64,
    cold_columns: Option<&[usize]>,
    row_group_rows: usize,
) -> io::Result<Manifest>
where
    I: IntoIterator<Item = Vec<u8>>,
{
    validate_config(cfg, max_cardinality)?;
    if !matches!(width, 1 | 2 | 4 | 8) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported token width",
        ));
    }

    let hot_columns = if let Some(cold) = cold_columns {
        if width != 4 || row_group_rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hybrid storage currently requires u32 tokens and row_group_rows > 0",
            ));
        }
        hybrid_partition(cfg.columns, cold)?
    } else {
        (0..cfg.columns).collect::<Vec<_>>()
    };

    let root = root.as_ref();
    let canonical = root.join("canonical");
    let routing = root.join("routing");
    let temp = root.join("temp");
    fs::create_dir_all(&canonical)?;
    fs::create_dir_all(&routing)?;
    fs::create_dir_all(&temp)?;

    let spool_paths: Vec<PathBuf> = (0..cfg.hierarchies.len())
        .map(|index| temp.join(format!("h{index:04}.raw")))
        .collect();
    let mut spools: Vec<BufWriter<File>> = spool_paths
        .iter()
        .map(File::create)
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .map(BufWriter::new)
        .collect();

    let row_bytes = cfg
        .columns
        .checked_mul(width)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "row size overflow"))?;
    let page_bytes = cfg
        .page_rows
        .checked_mul(row_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "page size overflow"))?;
    let mut carry: Vec<u8> = Vec::with_capacity(page_bytes * 2);
    let mut segments = Vec::new();
    let mut payloads = Vec::<ParquetPayloadFile>::new();
    let mut page_id = 0u32;
    let mut row_start = 0u64;
    let mut segment_number = 0usize;

    let mut emit_segment = |data: &[u8], final_partial: bool| -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if data.len() % row_bytes != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch payload is not whole rows",
            ));
        }
        let rows = data.len() / row_bytes;
        if !final_partial && rows % cfg.page_rows != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "non-final segment is not page aligned",
            ));
        }
        let first_page = page_id;
        for page_start in (0..rows).step_by(cfg.page_rows) {
            let page_end = (page_start + cfg.page_rows).min(rows);
            let page = &data[page_start * row_bytes..page_end * row_bytes];
            for (hierarchy_index, spec) in cfg.hierarchies.iter().enumerate() {
                for key in page_keys(page, cfg.columns, width, spec, &cfg.cardinalities)? {
                    write_record(&mut spools[hierarchy_index], key, page_id)?;
                }
            }
            page_id = page_id.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "page id overflow")
            })?;
        }

        let segment_name = format!("segment-{segment_number:06}.lhr");
        if let Some(cold) = cold_columns {
            let parquet_name = format!("segment-{segment_number:06}.parquet");
            let mut hot_data = Vec::with_capacity(rows * hot_columns.len() * width);
            let mut cold_data = vec![Vec::<u32>::with_capacity(rows); cold.len()];
            for row in 0..rows {
                for &column in &hot_columns {
                    let token = read_token(data, row * cfg.columns + column, width) as u32;
                    hot_data.extend_from_slice(&token.to_le_bytes());
                }
                for (slot, &column) in cold.iter().enumerate() {
                    cold_data[slot]
                        .push(read_token(data, row * cfg.columns + column, width) as u32);
                }
            }
            let mut writer = ParquetTokenWriter::create(
                canonical.join(&parquet_name),
                cold,
                row_group_rows,
            )?;
            writer.write_columns(&cold_data)?;
            let written = writer.finish()?;
            if written != rows as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "hybrid Parquet sidecar row count mismatch",
                ));
            }
            Segment::write_hybrid(
                canonical.join(&segment_name),
                rows as u64,
                cfg.columns as u32,
                width as u32,
                &hot_columns,
                cold,
                row_group_rows,
                &parquet_name,
                &hot_data,
            )?;
            payloads.push(ParquetPayloadFile {
                file: format!("canonical/{parquet_name}"),
                row_start,
                rows: rows as u64,
            });
        } else {
            Segment::write(
                canonical.join(&segment_name),
                rows as u64,
                cfg.columns as u32,
                width as u32,
                data,
            )?;
        }
        segment_number += 1;
        segments.push(SegmentMeta {
            file: segment_name,
            row_start,
            rows: rows as u64,
            first_page,
        });
        row_start += rows as u64;
        Ok(())
    };

    for batch in batches {
        if batch.len() % row_bytes != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch payload is not whole rows",
            ));
        }
        carry.extend_from_slice(&batch);
        let full = carry.len() / page_bytes * page_bytes;
        if full > 0 {
            let ready = carry[..full].to_vec();
            carry.drain(..full);
            emit_segment(&ready, false)?;
        }
    }
    if !carry.is_empty() {
        emit_segment(&carry, true)?;
    }
    drop(emit_segment);
    for spool in &mut spools {
        spool.flush()?;
    }
    drop(spools);

    let mut hierarchies = Vec::new();
    for (index, spec) in cfg.hierarchies.iter().enumerate() {
        let sparse_tmp = routing.join(format!("h{index:04}.sparse.tmp"));
        let entries = external_sort(&spool_paths[index], &sparse_tmp, cfg.max_sort_records)?;
        let _ = fs::remove_file(&spool_paths[index]);
        let space = keyspace(spec, &cfg.cardinalities)?;
        let sparse_bytes = entries.saturating_mul(12);
        let bitmap_bytes = BitmapHierarchy::estimated_bytes(space, page_id).unwrap_or(u64::MAX);
        if bitmap_bytes < sparse_bytes && bitmap_bytes <= MAX_AUTO_BITMAP_BYTES {
            let file = format!("h{index:04}.bit");
            BitmapHierarchy::build_from_sparse(&sparse_tmp, routing.join(&file), space, page_id)?;
            fs::remove_file(&sparse_tmp)?;
            hierarchies.push(HierarchyMeta {
                file,
                columns: spec.columns.clone(),
                entries,
                kind: "bitmap".into(),
                keyspace: space,
            });
        } else {
            let file = format!("h{index:04}.bin");
            fs::rename(&sparse_tmp, routing.join(&file))?;
            hierarchies.push(HierarchyMeta {
                file,
                columns: spec.columns.clone(),
                entries,
                kind: "sparse".into(),
                keyspace: space,
            });
        }
    }

    let manifest = Manifest {
        format: "LHR/1".into(),
        rows: row_start,
        columns: cfg.columns,
        page_rows: cfg.page_rows,
        pages: page_id,
        cardinalities: cfg.cardinalities.clone(),
        segments,
        hierarchies,
    };
    let tmp_manifest = root.join("manifest.json.tmp");
    fs::write(
        &tmp_manifest,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    )?;
    fs::rename(tmp_manifest, root.join("manifest.json"))?;

    if let Some(cold) = cold_columns {
        let mut layout = StorageLayout::hybrid(hot_columns, cold.to_vec(), payloads);
        layout.row_group_rows = row_group_rows;
        layout.validate(cfg.columns, row_start)?;
        write_storage_layout(root, &layout)?;
    }
    Ok(manifest)
}

pub fn build_u8_batches<I>(
    batches: I,
    root: impl AsRef<Path>,
    cfg: &BuildConfig,
) -> io::Result<Manifest>
where
    I: IntoIterator<Item = Vec<u8>>,
{
    build_encoded_batches(batches, root, cfg, 1, 256, None, 0)
}

pub fn build_u32_batches<I>(
    batches: I,
    root: impl AsRef<Path>,
    cfg: &BuildConfig,
) -> io::Result<Manifest>
where
    I: IntoIterator<Item = Vec<u32>>,
{
    let encoded = batches.into_iter().map(|batch| {
        let mut bytes = Vec::with_capacity(batch.len() * 4);
        for token in batch {
            bytes.extend_from_slice(&token.to_le_bytes());
        }
        bytes
    });
    build_encoded_batches(
        encoded,
        root,
        cfg,
        4,
        u32::MAX as u64 + 1,
        None,
        0,
    )
}

/// Build the same logical LHR/1 dataset while keeping selected cold columns in Parquet token
/// sidecars. Hot columns remain fixed-width mmap segments, and exact/page indexes are still built
/// from the complete logical token stream before the physical split.
pub fn build_hybrid_u32_batches<I>(
    batches: I,
    root: impl AsRef<Path>,
    cfg: &BuildConfig,
    cold_columns: &[usize],
    row_group_rows: usize,
) -> io::Result<Manifest>
where
    I: IntoIterator<Item = Vec<u32>>,
{
    let encoded = batches.into_iter().map(|batch| {
        let mut bytes = Vec::with_capacity(batch.len() * 4);
        for token in batch {
            bytes.extend_from_slice(&token.to_le_bytes());
        }
        bytes
    });
    build_encoded_batches(
        encoded,
        root,
        cfg,
        4,
        u32::MAX as u64 + 1,
        Some(cold_columns),
        row_group_rows,
    )
}

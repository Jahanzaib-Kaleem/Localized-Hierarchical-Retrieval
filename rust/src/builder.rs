use crate::{external::external_sort, manifest::{HierarchyMeta, Manifest, SegmentMeta}, Segment};
use std::{fs::{self, File}, io::{self, BufWriter, Write}, path::{Path, PathBuf}};

#[derive(Clone, Debug)]
pub struct HierarchySpec { pub columns: Vec<usize> }

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

fn page_keys(page: &[u8], columns: usize, spec: &HierarchySpec, card: &[u64]) -> io::Result<Vec<u64>> {
    let rows = page.len() / columns;
    let mut keys = Vec::with_capacity(rows);
    for r in 0..rows {
        let base = r * columns;
        let mut key = 0u64;
        for &c in &spec.columns {
            let value = page[base + c] as u64;
            let radix = card[c];
            if value >= radix { return Err(io::Error::new(io::ErrorKind::InvalidData, "token outside declared cardinality")); }
            key = key.checked_mul(radix).and_then(|x| x.checked_add(value)).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hierarchy key overflow"))?;
        }
        keys.push(key);
    }
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

pub fn build_u8_batches<I>(batches: I, root: impl AsRef<Path>, cfg: &BuildConfig) -> io::Result<Manifest>
where I: IntoIterator<Item = Vec<u8>> {
    if cfg.columns == 0 || cfg.page_rows == 0 || cfg.cardinalities.len() != cfg.columns || cfg.max_sort_records == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid build config"));
    }
    if cfg.cardinalities.iter().any(|&x| x == 0 || x > 256) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "u8 builder requires cardinalities in 1..=256"));
    }
    if cfg.hierarchies.iter().any(|h| h.columns.is_empty() || h.columns.iter().any(|&c| c >= cfg.columns)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid hierarchy columns"));
    }
    let root = root.as_ref();
    let canonical = root.join("canonical"); let routing = root.join("routing"); let temp = root.join("temp");
    fs::create_dir_all(&canonical)?; fs::create_dir_all(&routing)?; fs::create_dir_all(&temp)?;

    let spool_paths: Vec<PathBuf> = (0..cfg.hierarchies.len()).map(|i| temp.join(format!("h{i:04}.raw"))).collect();
    let mut spools: Vec<BufWriter<File>> = spool_paths.iter().map(File::create).collect::<io::Result<Vec<_>>>()?.into_iter().map(BufWriter::new).collect();

    let page_bytes = cfg.page_rows.checked_mul(cfg.columns).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "page size overflow"))?;
    let mut carry: Vec<u8> = Vec::with_capacity(page_bytes * 2);
    let mut segments = Vec::new(); let mut page_id = 0u32; let mut row_start = 0u64; let mut seg_no = 0usize;

    let mut emit_segment = |data: &[u8], final_partial: bool| -> io::Result<()> {
        if data.is_empty() { return Ok(()); }
        if data.len() % cfg.columns != 0 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "batch payload is not whole rows")); }
        let rows = data.len() / cfg.columns;
        if !final_partial && rows % cfg.page_rows != 0 { return Err(io::Error::new(io::ErrorKind::InvalidData, "non-final segment is not page aligned")); }
        let first_page = page_id;
        for page_start in (0..rows).step_by(cfg.page_rows) {
            let page_end = (page_start + cfg.page_rows).min(rows);
            let page = &data[page_start * cfg.columns..page_end * cfg.columns];
            for (hi, spec) in cfg.hierarchies.iter().enumerate() {
                for key in page_keys(page, cfg.columns, spec, &cfg.cardinalities)? { write_record(&mut spools[hi], key, page_id)?; }
            }
            page_id = page_id.checked_add(1).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "page id overflow"))?;
        }
        let name = format!("segment-{seg_no:06}.lhr"); seg_no += 1;
        Segment::write(canonical.join(&name), rows as u64, cfg.columns as u32, 1, data)?;
        segments.push(SegmentMeta { file: name, row_start, rows: rows as u64, first_page });
        row_start += rows as u64;
        Ok(())
    };

    for batch in batches {
        if batch.len() % cfg.columns != 0 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "batch payload is not whole rows")); }
        carry.extend_from_slice(&batch);
        let full = carry.len() / page_bytes * page_bytes;
        if full > 0 {
            let ready = carry[..full].to_vec();
            carry.drain(..full);
            emit_segment(&ready, false)?;
        }
    }
    if !carry.is_empty() { emit_segment(&carry, true)?; }
    drop(emit_segment);
    for s in &mut spools { s.flush()?; }
    drop(spools);

    let mut hierarchies = Vec::new();
    for (i, spec) in cfg.hierarchies.iter().enumerate() {
        let file = format!("h{i:04}.bin");
        let entries = external_sort(&spool_paths[i], routing.join(&file), cfg.max_sort_records)?;
        let _ = fs::remove_file(&spool_paths[i]);
        hierarchies.push(HierarchyMeta { file, columns: spec.columns.clone(), entries });
    }

    let manifest = Manifest { format: "LHR/1".into(), rows: row_start, columns: cfg.columns, page_rows: cfg.page_rows, pages: page_id, cardinalities: cfg.cardinalities.clone(), segments, hierarchies };
    let tmp_manifest = root.join("manifest.json.tmp");
    fs::write(&tmp_manifest, serde_json::to_vec_pretty(&manifest).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?)?;
    fs::rename(tmp_manifest, root.join("manifest.json"))?;
    Ok(manifest)
}

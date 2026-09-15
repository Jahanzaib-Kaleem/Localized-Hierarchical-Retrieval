use crate::{
    bitslice_postings::BitSlicePostingHierarchy,
    builder::HierarchySpec,
    delta_postings::DeltaPostingHierarchy,
    dense_postings::{count_unique_sorted, DensePostingHierarchy},
    external::external_sort,
    manifest::{HierarchyMeta, Manifest},
    postings::PostingHierarchy,
    Segment,
};
use std::{
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::Path,
};

fn keyspace(spec: &HierarchySpec, card: &[u64]) -> io::Result<u64> {
    spec.columns.iter().try_fold(1u64, |a, &c| {
        a.checked_mul(card[c])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "keyspace overflow"))
    })
}

fn write_record<W: Write>(w: &mut W, key: u64, row: u32) -> io::Result<()> {
    w.write_all(&key.to_le_bytes())?;
    w.write_all(&row.to_le_bytes())
}

pub fn add_exact_hierarchies(
    root: impl AsRef<Path>,
    specs: &[HierarchySpec],
    max_sort_records: usize,
) -> io::Result<Manifest> {
    if max_sort_records == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "max_sort_records must be > 0",
        ));
    }
    let root = root.as_ref();
    let raw = fs::read(root.join("manifest.json"))?;
    let mut manifest: Manifest = serde_json::from_slice(&raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if manifest.rows > u32::MAX as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "exact postings v1 require <= u32::MAX rows",
        ));
    }

    for spec in specs {
        if spec.columns.is_empty() || spec.columns.iter().any(|&c| c >= manifest.columns) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid exact hierarchy columns",
            ));
        }
        if manifest.hierarchies.iter().any(|h| {
            matches!(
                h.kind.as_str(),
                "postings" | "densepost" | "deltapost" | "bitslice"
            ) && h.columns == spec.columns
        }) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "exact hierarchy already exists",
            ));
        }
    }

    let temp = root.join("temp");
    let routing = root.join("routing");
    fs::create_dir_all(&temp)?;
    fs::create_dir_all(&routing)?;
    let base = manifest.hierarchies.len();
    let spool_paths: Vec<_> = (0..specs.len())
        .map(|i| temp.join(format!("exact-{:04}.raw", base + i)))
        .collect();
    let mut writers: Vec<BufWriter<File>> = spool_paths
        .iter()
        .map(File::create)
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .map(BufWriter::new)
        .collect();

    for meta in &manifest.segments {
        let seg = Segment::open(root.join("canonical").join(&meta.file))?;
        for local in 0..seg.rows() {
            let row = (meta.row_start + local as u64) as u32;
            for (i, spec) in specs.iter().enumerate() {
                let mut key = 0u64;
                for &c in &spec.columns {
                    let value = seg.value(local, c).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "segment column missing")
                    })?;
                    let radix = manifest.cardinalities[c];
                    if value >= radix {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "token outside cardinality",
                        ));
                    }
                    key = key
                        .checked_mul(radix)
                        .and_then(|x| x.checked_add(value))
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "key overflow")
                        })?;
                }
                write_record(&mut writers[i], key, row)?;
            }
        }
    }
    for w in &mut writers {
        w.flush()?;
    }
    drop(writers);

    for (i, spec) in specs.iter().enumerate() {
        let sorted = temp.join(format!("exact-{:04}.sorted", base + i));
        let entries = external_sort(&spool_paths[i], &sorted, max_sort_records)?;
        fs::remove_file(&spool_paths[i])?;
        if entries != manifest.rows {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exact hierarchy lost or duplicated rows",
            ));
        }

        let space = keyspace(spec, &manifest.cardinalities)?;
        let unique = count_unique_sorted(&sorted)?;
        let sparse_bytes = 40u64
            .saturating_add(unique.saturating_mul(24))
            .saturating_add(manifest.rows.saturating_mul(4));
        let dense_bytes =
            DensePostingHierarchy::estimated_bytes(space, manifest.rows).unwrap_or(u64::MAX);
        let bitslice_bytes = if spec.columns.len() == 1 {
            BitSlicePostingHierarchy::estimated_bytes(space, manifest.rows).unwrap_or(u64::MAX)
        } else {
            u64::MAX
        };

        // Delta size depends on row locality, so measure the real file before choosing.
        let delta_file = format!("h{:04}.dlt", base + i);
        let delta_path = routing.join(&delta_file);
        DeltaPostingHierarchy::build_from_sorted(&sorted, &delta_path)?;
        let delta_bytes = fs::metadata(&delta_path)?.len();

        let (file, kind) = if bitslice_bytes <= delta_bytes
            && bitslice_bytes <= dense_bytes
            && bitslice_bytes <= sparse_bytes
        {
            fs::remove_file(&delta_path)?;
            let file = format!("h{:04}.bsl", base + i);
            BitSlicePostingHierarchy::build_from_sorted(
                &sorted,
                routing.join(&file),
                space,
                manifest.rows,
            )?;
            (file, "bitslice".to_string())
        } else if delta_bytes <= dense_bytes && delta_bytes <= sparse_bytes {
            (delta_file, "deltapost".to_string())
        } else if dense_bytes < sparse_bytes {
            fs::remove_file(&delta_path)?;
            let file = format!("h{:04}.dpost", base + i);
            DensePostingHierarchy::build_from_sorted(
                &sorted,
                routing.join(&file),
                space,
                manifest.rows,
            )?;
            (file, "densepost".to_string())
        } else {
            fs::remove_file(&delta_path)?;
            let file = format!("h{:04}.post", base + i);
            PostingHierarchy::build_from_sorted(&sorted, routing.join(&file), manifest.rows)?;
            (file, "postings".to_string())
        };
        fs::remove_file(sorted)?;
        manifest.hierarchies.push(HierarchyMeta {
            file,
            columns: spec.columns.clone(),
            entries,
            kind,
            keyspace: space,
        });
    }

    let tmp = root.join("manifest.json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
    )?;
    fs::rename(tmp, root.join("manifest.json"))?;
    Ok(manifest)
}

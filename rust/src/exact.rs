use crate::{
    bitslice_postings::BitSlicePostingHierarchy,
    builder::HierarchySpec,
    delta_postings::DeltaPostingHierarchy,
    dense_postings::{count_unique_sorted, DensePostingHierarchy},
    external::external_sort,
    flat_postings::FlatPostingHierarchy,
    manifest::{HierarchyMeta, Manifest},
    postings::PostingHierarchy,
    Segment,
};
use std::{
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::Path,
};

const BITSLICE_STORAGE_BUDGET_MULTIPLIER: u64 = 2;

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

fn hierarchy_number(file: &str) -> Option<usize> {
    let rest = file.strip_prefix('h')?;
    let digits = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    rest[..digits].parse().ok()
}

fn next_hierarchy_number(manifest: &Manifest) -> io::Result<usize> {
    manifest
        .hierarchies
        .iter()
        .filter_map(|hierarchy| hierarchy_number(&hierarchy.file))
        .max()
        .map(|number| {
            number
                .checked_add(1)
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "hierarchy number overflow"))
        })
        .unwrap_or(Ok(0))
}

// Bit-slices are excellent for low/moderate cardinalities because composition becomes
// word-wise equality-mask work instead of decoding very large row-id postings. Do not
// extend them into sparse high-cardinality fields: the all-row word scan then dominates.
// An 8x word-work allowance admits card~64 while still excluding card~1K+ at our scales.
fn bitslice_query_ok(keyspace: u64, rows: u64) -> bool {
    if keyspace == 0 {
        return false;
    }
    let bits = if keyspace <= 1 {
        0
    } else {
        (64 - (keyspace - 1).leading_zeros()) as u64
    };
    let words = rows.saturating_add(63) / 64;
    let bit_word_ops = words.saturating_mul(bits);
    let avg_posting_rows = rows
        .saturating_add(keyspace.saturating_sub(1))
        .checked_div(keyspace)
        .unwrap_or(0);
    bit_word_ops <= avg_posting_rows.saturating_mul(8)
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
                "postings" | "densepost" | "deltapost" | "bitslice" | "flatpost"
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
    // Hierarchies can now be dropped administratively. Manifest length is therefore not a safe
    // file-number allocator: reusing a lower number could overwrite a surviving index. Always
    // allocate above the greatest hierarchy number already present.
    let base = next_hierarchy_number(&manifest)?;
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
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "key overflow"))?;
                }
                write_record(&mut writers[i], key, row)?;
            }
        }
    }
    for writer in &mut writers {
        writer.flush()?;
    }
    drop(writers);

    for (i, spec) in specs.iter().enumerate() {
        let number = base
            .checked_add(i)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "hierarchy number overflow"))?;
        let sorted = temp.join(format!("exact-{number:04}.sorted"));
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
        let flat_bytes = FlatPostingHierarchy::estimated_bytes(manifest.rows).unwrap_or(u64::MAX);
        let bitslice_bytes = if spec.columns.len() == 1 && bitslice_query_ok(space, manifest.rows) {
            BitSlicePostingHierarchy::estimated_bytes(space, manifest.rows).unwrap_or(u64::MAX)
        } else {
            u64::MAX
        };

        // Delta size depends on row locality, so measure the real file before choosing.
        let delta_file = format!("h{number:04}.dlt");
        let delta_path = routing.join(&delta_file);
        DeltaPostingHierarchy::build_from_sorted(&sorted, &delta_path)?;
        let delta_bytes = fs::metadata(&delta_path)?.len();

        let best_non_bitslice = delta_bytes
            .min(dense_bytes)
            .min(sparse_bytes)
            .min(flat_bytes);
        // For low-cardinality fields, a small storage premium is worthwhile because two
        // bit-sliced predicates can be composed directly as word masks without materializing
        // huge postings. The query-work guard above keeps this speed budget away from sparse
        // high-cardinality fields.
        let prefer_bitslice = bitslice_bytes != u64::MAX
            && bitslice_bytes
                <= best_non_bitslice.saturating_mul(BITSLICE_STORAGE_BUDGET_MULTIPLIER);

        let (file, kind) = if prefer_bitslice {
            fs::remove_file(&delta_path)?;
            let file = format!("h{number:04}.bsl");
            BitSlicePostingHierarchy::build_from_sorted(
                &sorted,
                routing.join(&file),
                space,
                manifest.rows,
            )?;
            (file, "bitslice".to_string())
        } else if flat_bytes <= delta_bytes
            && flat_bytes <= dense_bytes
            && flat_bytes <= sparse_bytes
        {
            fs::remove_file(&delta_path)?;
            let file = format!("h{number:04}.flat");
            FlatPostingHierarchy::build_from_sorted(&sorted, routing.join(&file), manifest.rows)?;
            (file, "flatpost".to_string())
        } else if delta_bytes <= dense_bytes && delta_bytes <= sparse_bytes {
            (delta_file, "deltapost".to_string())
        } else if dense_bytes < sparse_bytes {
            fs::remove_file(&delta_path)?;
            let file = format!("h{number:04}.dpost");
            DensePostingHierarchy::build_from_sorted(
                &sorted,
                routing.join(&file),
                space,
                manifest.rows,
            )?;
            (file, "densepost".to_string())
        } else {
            fs::remove_file(&delta_path)?;
            let file = format!("h{number:04}.post");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_numbers_ignore_manifest_holes() {
        let manifest = Manifest {
            format: "LHR/1".into(),
            rows: 1,
            columns: 2,
            page_rows: 1,
            pages: 1,
            cardinalities: vec![2, 2],
            segments: vec![],
            hierarchies: vec![
                HierarchyMeta {
                    file: "h0000.dlt".into(),
                    columns: vec![0],
                    entries: 1,
                    kind: "deltapost".into(),
                    keyspace: 2,
                },
                HierarchyMeta {
                    file: "h0007.flat".into(),
                    columns: vec![0, 1],
                    entries: 1,
                    kind: "flatpost".into(),
                    keyspace: 4,
                },
            ],
        };
        assert_eq!(next_hierarchy_number(&manifest).unwrap(), 8);
    }
}

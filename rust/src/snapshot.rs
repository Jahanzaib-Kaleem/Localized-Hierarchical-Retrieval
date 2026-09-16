use crate::{resolve_dataset_root, vacuum_generations, VacuumReport};
use fs2::FileExt;
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

const READERS_DIR: &str = "READERS";

pub struct SnapshotLease {
    path: PathBuf,
    generation_id: Option<u64>,
    _lock: Option<File>,
}

impl SnapshotLease {
    /// Resolve CURRENT once and hold a shared OS lock for the selected generation. Publication of
    /// a newer generation does not affect this snapshot; lease-aware vacuum will not unlink it.
    pub fn acquire(root: impl AsRef<Path>) -> io::Result<Self> {
        let catalog = root.as_ref();
        let path = resolve_dataset_root(catalog)?;
        let generation_id = path
            .file_name()
            .and_then(|x| x.to_str())
            .filter(|x| x.len() == 20 && x.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|x| x.parse::<u64>().ok())
            .filter(|_| {
                path.parent()
                    .and_then(|x| x.file_name())
                    .and_then(|x| x.to_str())
                    == Some("generations")
            });
        let lock = if let Some(id) = generation_id {
            let dir = catalog.join(READERS_DIR);
            fs::create_dir_all(&dir)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(dir.join(format!("{id:020}.lock")))?;
            FileExt::lock_shared(&file)?;
            Some(file)
        } else {
            None
        };
        Ok(Self {
            path,
            generation_id,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn generation_id(&self) -> Option<u64> {
        self.generation_id
    }
}

pub fn leased_generation_ids(root: impl AsRef<Path>) -> io::Result<Vec<u64>> {
    let dir = root.as_ref().join(READERS_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut leased = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(raw) = name.strip_suffix(".lock") else {
            continue;
        };
        if raw.len() != 20 || !raw.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(id) = raw.parse::<u64>() else {
            continue;
        };
        let file = OpenOptions::new().read(true).write(true).open(entry.path())?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => leased.push(id),
            Err(error) => return Err(error),
        }
    }
    leased.sort_unstable();
    Ok(leased)
}

#[derive(Debug, Clone, Serialize)]
pub struct SafeVacuumReport {
    pub leased_generations: Vec<u64>,
    pub vacuum: VacuumReport,
}

pub fn vacuum_with_reader_leases(
    root: impl AsRef<Path>,
    retain_newest: usize,
    protected: &[u64],
) -> io::Result<SafeVacuumReport> {
    let root = root.as_ref();
    let leased = leased_generation_ids(root)?;
    let mut keep: BTreeSet<u64> = protected.iter().copied().collect();
    keep.extend(leased.iter().copied());
    let keep: Vec<_> = keep.into_iter().collect();
    let vacuum = vacuum_generations(root, retain_newest, &keep)?;
    Ok(SafeVacuumReport {
        leased_generations: leased,
        vacuum,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_u8_batches, begin_generation, publish_generation, BuildConfig};

    #[test]
    fn active_snapshot_is_reported_as_leased() {
        let dir = tempfile::tempdir().unwrap();
        let stage = begin_generation(dir.path()).unwrap();
        let cfg = BuildConfig {
            columns: 2,
            cardinalities: vec![2, 2],
            page_rows: 2,
            hierarchies: vec![],
            max_sort_records: 32,
        };
        build_u8_batches(vec![vec![0u8, 1u8, 0u8, 1u8]], &stage.path, &cfg).unwrap();
        publish_generation(stage).unwrap();
        let lease = SnapshotLease::acquire(dir.path()).unwrap();
        assert_eq!(leased_generation_ids(dir.path()).unwrap(), vec![1]);
        drop(lease);
        assert!(leased_generation_ids(dir.path()).unwrap().is_empty());
    }
}

use crate::{seal_dataset, verify_dataset, verify_integrity_metadata};
use fs2::FileExt;
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

const CURRENT_FILE: &str = "CURRENT";
const GENERATIONS_DIR: &str = "generations";
const WRITER_LOCK: &str = "WRITER.lock";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GenerationInfo {
    pub id: u64,
    pub path: PathBuf,
    pub current: bool,
    pub sealed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VacuumReport {
    pub kept: Vec<u64>,
    pub deleted: Vec<u64>,
    pub stale_paths_removed: usize,
    pub bytes_reclaimed: u64,
}

pub struct StagedGeneration {
    pub id: u64,
    pub path: PathBuf,
    catalog_root: PathBuf,
    _writer_lock: File,
}

fn parse_generation_id(raw: &str) -> io::Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CURRENT contains an invalid generation id",
        ));
    }
    raw.parse::<u64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn generation_name(id: u64) -> String {
    format!("{id:020}")
}

fn generation_path(root: &Path, id: u64) -> PathBuf {
    root.join(GENERATIONS_DIR).join(generation_name(id))
}

fn current_id(root: &Path) -> io::Result<Option<u64>> {
    let path = root.join(CURRENT_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::str::from_utf8(&fs::read(path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        .to_owned();
    Ok(Some(parse_generation_id(&text)?))
}

fn acquire_writer_lock(root: &Path) -> io::Result<File> {
    fs::create_dir_all(root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(root.join(WRITER_LOCK))?;
    lock.try_lock_exclusive().map_err(|e| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("another LHR writer owns the catalog lock: {e}"),
        )
    })?;
    Ok(lock)
}

fn atomic_write_current(root: &Path, id: u64) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let target = root.join(CURRENT_FILE);
    let tmp = root.join(format!(".CURRENT.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        writeln!(file, "{}", generation_name(id))?;
        file.sync_all()?;
        fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn directory_bytes(path: &Path) -> io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total = total.saturating_add(directory_bytes(&entry.path())?);
        } else if meta.is_file() {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

/// Resolve either a generation catalog root or a legacy single-generation dataset root.
pub fn resolve_dataset_root(root: impl AsRef<Path>) -> io::Result<PathBuf> {
    let root = root.as_ref();
    if let Some(id) = current_id(root)? {
        let path = generation_path(root, id);
        if !path.join("manifest.json").is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("CURRENT points to missing generation {}", generation_name(id)),
            ));
        }
        return Ok(path);
    }
    if root.join("manifest.json").is_file() {
        return Ok(root.to_path_buf());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no CURRENT generation and no legacy manifest.json",
    ))
}

pub fn list_generations(root: impl AsRef<Path>) -> io::Result<Vec<GenerationInfo>> {
    let root = root.as_ref();
    let current = current_id(root)?;
    let dir = root.join(GENERATIONS_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() != 20 || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(id) = name.parse::<u64>() else {
            continue;
        };
        let path = entry.path();
        out.push(GenerationInfo {
            id,
            current: current == Some(id),
            sealed: path.join("integrity.json").is_file(),
            path,
        });
    }
    out.sort_by_key(|x| x.id);
    Ok(out)
}

fn next_generation_id(root: &Path) -> io::Result<u64> {
    let max_listed = list_generations(root)?.into_iter().map(|x| x.id).max();
    let max_current = current_id(root)?;
    max_listed
        .into_iter()
        .chain(max_current)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "generation id overflow"))
}

pub fn begin_generation(root: impl AsRef<Path>) -> io::Result<StagedGeneration> {
    let root = root.as_ref();
    fs::create_dir_all(root.join(GENERATIONS_DIR))?;
    let lock = acquire_writer_lock(root)?;

    let id = next_generation_id(root)?;
    let path = root
        .join(GENERATIONS_DIR)
        .join(format!(".staging-{}-{}", generation_name(id), std::process::id()));
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "staging generation already exists",
        ));
    }
    fs::create_dir(&path)?;
    Ok(StagedGeneration {
        id,
        path,
        catalog_root: root.to_path_buf(),
        _writer_lock: lock,
    })
}

pub fn publish_generation(stage: StagedGeneration) -> io::Result<GenerationInfo> {
    let mut report = verify_dataset(&stage.path)?;
    if !report.valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("staged generation is invalid: {}", report.errors.join("; ")),
        ));
    }
    if !stage.path.join("integrity.json").is_file() {
        seal_dataset(&stage.path)?;
        report = verify_dataset(&stage.path)?;
        if !report.valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sealed generation failed verification: {}", report.errors.join("; ")),
            ));
        }
    }

    let final_path = generation_path(&stage.catalog_root, stage.id);
    if final_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "generation id already exists",
        ));
    }
    fs::rename(&stage.path, &final_path)?;
    atomic_write_current(&stage.catalog_root, stage.id)?;

    Ok(GenerationInfo {
        id: stage.id,
        path: final_path,
        current: true,
        sealed: true,
    })
}

/// Publish a generation whose integrity manifest was composed from already verified immutable
/// parts. This performs structural checks plus file-set/size validation without re-hashing every
/// unchanged byte. Full checksum verification remains available through `verify_dataset` and
/// recovery.
pub fn publish_presealed_generation(stage: StagedGeneration) -> io::Result<GenerationInfo> {
    if !stage.path.join("integrity.json").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "presealed publication requires integrity.json",
        ));
    }
    let report = verify_integrity_metadata(&stage.path)?;
    if !report.valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "presealed staged generation is invalid: {}",
                report.errors.join("; ")
            ),
        ));
    }

    let final_path = generation_path(&stage.catalog_root, stage.id);
    if final_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "generation id already exists",
        ));
    }
    fs::rename(&stage.path, &final_path)?;
    atomic_write_current(&stage.catalog_root, stage.id)?;

    Ok(GenerationInfo {
        id: stage.id,
        path: final_path,
        current: true,
        sealed: true,
    })
}

pub fn abandon_generation(stage: StagedGeneration) -> io::Result<()> {
    if stage.path.exists() {
        fs::remove_dir_all(&stage.path)?;
    }
    Ok(())
}

pub fn rollback_generation(root: impl AsRef<Path>, id: u64) -> io::Result<GenerationInfo> {
    let root = root.as_ref();
    let _lock = acquire_writer_lock(root)?;
    let path = generation_path(root, id);
    if !path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("generation {} does not exist", generation_name(id)),
        ));
    }
    let report = verify_dataset(&path)?;
    if !report.valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("generation cannot be activated: {}", report.errors.join("; ")),
        ));
    }
    atomic_write_current(root, id)?;
    Ok(GenerationInfo {
        id,
        path: path.clone(),
        current: true,
        sealed: path.join("integrity.json").is_file(),
    })
}

pub fn vacuum_generations(
    root: impl AsRef<Path>,
    retain_newest: usize,
    protected: &[u64],
) -> io::Result<VacuumReport> {
    let root = root.as_ref();
    let _lock = acquire_writer_lock(root)?;
    let current = current_id(root)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "generation vacuum requires a catalog CURRENT")
    })?;
    let current_path = generation_path(root, current);
    let report = verify_dataset(&current_path)?;
    if !report.valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("refusing to vacuum while CURRENT is invalid: {}", report.errors.join("; ")),
        ));
    }

    let generations = list_generations(root)?;
    let published: BTreeSet<u64> = generations.iter().map(|x| x.id).collect();
    for &id in protected {
        if !published.contains(&id) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("protected generation {} does not exist", generation_name(id)),
            ));
        }
    }

    let mut keep = BTreeSet::new();
    keep.insert(current);
    keep.extend(protected.iter().copied());
    for generation in generations.iter().rev().take(retain_newest) {
        keep.insert(generation.id);
    }

    let mut deleted = Vec::new();
    let mut bytes_reclaimed = 0u64;
    for generation in &generations {
        if keep.contains(&generation.id) {
            continue;
        }
        bytes_reclaimed = bytes_reclaimed.saturating_add(directory_bytes(&generation.path)?);
        fs::remove_dir_all(&generation.path)?;
        deleted.push(generation.id);
    }

    let mut stale_paths_removed = 0usize;
    let generations_dir = root.join(GENERATIONS_DIR);
    if generations_dir.is_dir() {
        for entry in fs::read_dir(&generations_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && name.starts_with(".staging-") {
                bytes_reclaimed = bytes_reclaimed.saturating_add(directory_bytes(&entry.path())?);
                fs::remove_dir_all(entry.path())?;
                stale_paths_removed += 1;
            }
        }
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let stale_work = name.starts_with(".mutation-work-")
            || name.starts_with(".restore-work-")
            || name.starts_with(".delta-work-")
            || name.starts_with(".compaction-work-")
            || name.starts_with(".segmented-import-work-")
            || name.starts_with(".segmented-append-work-");
        if entry.file_type()?.is_dir() && stale_work {
            bytes_reclaimed = bytes_reclaimed.saturating_add(directory_bytes(&entry.path())?);
            fs::remove_dir_all(entry.path())?;
            stale_paths_removed += 1;
        }
    }

    Ok(VacuumReport {
        kept: keep.into_iter().collect(),
        deleted,
        stale_paths_removed,
        bytes_reclaimed,
    })
}

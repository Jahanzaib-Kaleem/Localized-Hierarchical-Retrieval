use crate::{seal_dataset, verify_dataset};
use fs2::FileExt;
use serde::Serialize;
use std::{
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

/// Begin an unpublished immutable generation while holding the catalog's writer lock.
/// Build canonical/index files inside `stage.path`, then call `publish_generation`.
pub fn begin_generation(root: impl AsRef<Path>) -> io::Result<StagedGeneration> {
    let root = root.as_ref();
    fs::create_dir_all(root.join(GENERATIONS_DIR))?;
    let lock_path = root.join(WRITER_LOCK);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)?;
    lock.try_lock_exclusive().map_err(|e| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("another LHR writer owns the catalog lock: {e}"),
        )
    })?;

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

/// Verify, seal, atomically install, and then publish a staged generation through CURRENT.
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

pub fn abandon_generation(stage: StagedGeneration) -> io::Result<()> {
    if stage.path.exists() {
        fs::remove_dir_all(&stage.path)?;
    }
    Ok(())
}

/// Atomically repoint CURRENT to an already-published, verified generation.
pub fn rollback_generation(root: impl AsRef<Path>, id: u64) -> io::Result<GenerationInfo> {
    let root = root.as_ref();
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

use crate::{dictionary_filename, read_schema, Dictionary, Engine, Manifest, Segment};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
};

const INTEGRITY_FORMAT: &str = "LHR-INTEGRITY/1";

#[derive(Debug, Clone, Serialize)]
pub struct DatasetStatus {
    pub format: String,
    pub rows: u64,
    pub columns: usize,
    pub pages: u32,
    pub segments: usize,
    pub hierarchies: usize,
    pub canonical_bytes: u64,
    pub routing_bytes: u64,
    pub total_bytes: u64,
    pub integrity_present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrityEntry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrityManifest {
    pub format: String,
    pub entries: Vec<IntegrityEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    pub valid: bool,
    pub checked_files: usize,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

fn json_error(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

fn read_manifest(root: &Path) -> io::Result<Manifest> {
    serde_json::from_slice(&fs::read(root.join("manifest.json"))?).map_err(json_error)
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

pub fn dataset_status(root: impl AsRef<Path>) -> io::Result<DatasetStatus> {
    let root = root.as_ref();
    let manifest = read_manifest(root)?;
    let canonical_bytes = directory_bytes(&root.join("canonical"))?;
    let routing_bytes = directory_bytes(&root.join("routing"))?;
    let total_bytes = directory_bytes(root)?;
    Ok(DatasetStatus {
        format: manifest.format,
        rows: manifest.rows,
        columns: manifest.columns,
        pages: manifest.pages,
        segments: manifest.segments.len(),
        hierarchies: manifest.hierarchies.len(),
        canonical_bytes,
        routing_bytes,
        total_bytes,
        integrity_present: root.join("integrity.json").is_file(),
    })
}

fn relative_string(root: &Path, path: &Path) -> io::Result<String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path outside dataset root"))?;
    Ok(rel
        .components()
        .map(|x| x.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn collect_stable_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let first = rel.components().next().map(|x| x.as_os_str().to_string_lossy());
            if first.as_deref() == Some("temp") {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "integrity.json" || name.ends_with(".tmp") || name.contains(".partial-") {
                continue;
            }
            let meta = entry.metadata()?;
            if meta.is_dir() {
                walk(root, &path, out)?;
            } else if meta.is_file() {
                out.push(path);
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort_by(|a, b| {
        relative_string(root, a)
            .unwrap_or_default()
            .cmp(&relative_string(root, b).unwrap_or_default())
    });
    Ok(out)
}

fn sha256_file(path: &Path) -> io::Result<(u64, String)> {
    let file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Ok((bytes, hex))
}

pub fn integrity_entry_for_file(
    root: impl AsRef<Path>,
    relative: impl AsRef<Path>,
) -> io::Result<IntegrityEntry> {
    let root = root.as_ref();
    let relative = relative.as_ref();
    if relative.is_absolute()
        || relative.components().any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "integrity entry path must be a normal relative path",
        ));
    }
    let path = root.join(relative);
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("integrity entry file is missing: {}", relative.display()),
        ));
    }
    let (bytes, sha256) = sha256_file(&path)?;
    Ok(IntegrityEntry {
        path: relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        bytes,
        sha256,
    })
}

pub fn install_integrity_manifest(
    root: impl AsRef<Path>,
    mut entries: Vec<IntegrityEntry>,
) -> io::Result<IntegrityManifest> {
    let root = root.as_ref();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    for pair in entries.windows(2) {
        if pair[0].path == pair[1].path {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate integrity entry {}", pair[0].path),
            ));
        }
    }

    let current = collect_stable_files(root)?;
    let current_set: BTreeSet<_> = current
        .iter()
        .map(|path| relative_string(root, path))
        .collect::<io::Result<_>>()?;
    let supplied_set: BTreeSet<_> = entries.iter().map(|entry| entry.path.clone()).collect();
    if current_set != supplied_set {
        let missing = current_set
            .difference(&supplied_set)
            .cloned()
            .collect::<Vec<_>>();
        let extra = supplied_set
            .difference(&current_set)
            .cloned()
            .collect::<Vec<_>>();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "composed integrity file set mismatch; missing={missing:?} extra={extra:?}"
            ),
        ));
    }

    for entry in &entries {
        let length = fs::metadata(root.join(&entry.path))?.len();
        if length != entry.bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "composed integrity size mismatch {}: {} != {}",
                    entry.path, length, entry.bytes
                ),
            ));
        }
    }

    let manifest = IntegrityManifest {
        format: INTEGRITY_FORMAT.into(),
        entries,
    };
    let data = serde_json::to_vec_pretty(&manifest).map_err(json_error)?;
    atomic_write(&root.join("integrity.json"), &data)?;
    Ok(manifest)
}

pub fn verify_integrity_metadata(root: impl AsRef<Path>) -> io::Result<VerificationReport> {
    let root = root.as_ref();
    let mut report = verify_dataset_structure(root)?;
    if !report.valid {
        return Ok(report);
    }
    let Some(seal) = read_integrity_manifest(root)? else {
        report.valid = false;
        report.errors.push("dataset has no integrity seal".into());
        return Ok(report);
    };
    if seal.format != INTEGRITY_FORMAT {
        report.valid = false;
        report
            .errors
            .push(format!("unsupported integrity format {}", seal.format));
        return Ok(report);
    }
    let current = collect_stable_files(root)?;
    let current_set: BTreeSet<_> = current
        .iter()
        .map(|path| relative_string(root, path))
        .collect::<io::Result<_>>()?;
    let sealed_set: BTreeSet<_> = seal.entries.iter().map(|entry| entry.path.clone()).collect();
    if current_set != sealed_set {
        report.valid = false;
        report.errors.push("integrity file set does not match dataset".into());
        return Ok(report);
    }
    for entry in &seal.entries {
        match fs::metadata(root.join(&entry.path)) {
            Ok(metadata) if metadata.len() == entry.bytes => report.checked_files += 1,
            Ok(metadata) => {
                report.valid = false;
                report.errors.push(format!(
                    "size mismatch {}: {} != {}",
                    entry.path,
                    metadata.len(),
                    entry.bytes
                ));
            }
            Err(error) => {
                report.valid = false;
                report.errors.push(format!("{}: {error}", entry.path));
            }
        }
    }
    Ok(report)
}

fn compute_integrity(root: &Path) -> io::Result<IntegrityManifest> {
    let mut entries = Vec::new();
    for path in collect_stable_files(root)? {
        let (bytes, sha256) = sha256_file(&path)?;
        entries.push(IntegrityEntry {
            path: relative_string(root, &path)?,
            bytes,
            sha256,
        });
    }
    Ok(IntegrityManifest {
        format: INTEGRITY_FORMAT.into(),
        entries,
    })
}

fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy();
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn read_integrity_manifest(root: impl AsRef<Path>) -> io::Result<Option<IntegrityManifest>> {
    let path = root.as_ref().join("integrity.json");
    if !path.exists() {
        return Ok(None);
    }
    let manifest = serde_json::from_slice(&fs::read(path)?).map_err(json_error)?;
    Ok(Some(manifest))
}

pub fn verify_dataset_structure(root: &Path) -> io::Result<VerificationReport> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut checked_files = 0usize;

    let manifest = match read_manifest(root) {
        Ok(x) => x,
        Err(e) => {
            return Ok(VerificationReport {
                valid: false,
                checked_files,
                errors: vec![format!("manifest: {e}")],
                warnings,
            })
        }
    };
    checked_files += 1;

    if !manifest.format.starts_with("LHR/") {
        errors.push(format!("unsupported format {}", manifest.format));
    }
    if manifest.columns == 0 || manifest.page_rows == 0 {
        errors.push("manifest has zero columns or page_rows".into());
    }
    if manifest.cardinalities.len() != manifest.columns {
        errors.push("cardinality count does not match columns".into());
    }
    if manifest.cardinalities.iter().any(|&x| x == 0) {
        errors.push("zero cardinality is invalid".into());
    }

    let mut expected_row = 0u64;
    let mut expected_page = 0u64;
    for segment in &manifest.segments {
        if segment.row_start != expected_row {
            errors.push(format!(
                "segment {} row_start {} != expected {}",
                segment.file, segment.row_start, expected_row
            ));
        }
        if segment.first_page as u64 != expected_page {
            errors.push(format!(
                "segment {} first_page {} != expected {}",
                segment.file, segment.first_page, expected_page
            ));
        }
        let path = root.join("canonical").join(&segment.file);
        match Segment::open(&path) {
            Ok(data) => {
                checked_files += 1;
                if data.rows() as u64 != segment.rows {
                    errors.push(format!("segment {} row count mismatch", segment.file));
                }
                if data.cols() != manifest.columns {
                    errors.push(format!("segment {} column count mismatch", segment.file));
                }
            }
            Err(e) => errors.push(format!("segment {}: {e}", segment.file)),
        }
        expected_row = expected_row.saturating_add(segment.rows);
        let pages = if segment.rows == 0 {
            0
        } else {
            (segment.rows + manifest.page_rows as u64 - 1) / manifest.page_rows as u64
        };
        expected_page = expected_page.saturating_add(pages);
    }
    if expected_row != manifest.rows {
        errors.push(format!("manifest rows {} != segment rows {}", manifest.rows, expected_row));
    }
    if expected_page != manifest.pages as u64 {
        errors.push(format!("manifest pages {} != segment pages {}", manifest.pages, expected_page));
    }

    match read_schema(root) {
        Ok(schema) => {
            if schema.columns.len() != manifest.columns {
                errors.push("schema column count does not match manifest".into());
            } else {
                for (column, spec) in schema.columns.iter().enumerate() {
                    let path = root.join("dictionaries").join(dictionary_filename(column));
                    match Dictionary::open(&path) {
                        Ok(dictionary) => {
                            checked_files += 1;
                            if dictionary.nullable() != spec.nullable {
                                errors.push(format!(
                                    "dictionary nullability mismatch for {}",
                                    spec.name
                                ));
                            }
                            if dictionary.cardinality() != manifest.cardinalities[column] {
                                errors.push(format!(
                                    "dictionary cardinality mismatch for {}",
                                    spec.name
                                ));
                            }
                            if let Err(error) = dictionary.verify_offsets() {
                                errors.push(format!("dictionary {}: {error}", spec.name));
                            }
                        }
                        Err(error) => errors.push(format!(
                            "dictionary {}: {error}",
                            spec.name
                        )),
                    }
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            warnings.push(
                "dataset has no logical schema; dictionary deep verification was skipped".into(),
            );
        }
        Err(error) => errors.push(format!("schema: {error}")),
    }

    for hierarchy in &manifest.hierarchies {
        if hierarchy.columns.is_empty()
            || hierarchy.columns.iter().any(|&c| c >= manifest.columns)
        {
            errors.push(format!("hierarchy {} has invalid columns", hierarchy.file));
        }
        let path = root.join("routing").join(&hierarchy.file);
        if path.is_file() {
            checked_files += 1;
        } else {
            errors.push(format!("missing hierarchy file {}", hierarchy.file));
        }
    }

    if errors.is_empty() {
        if let Err(e) = Engine::open(root) {
            errors.push(format!("engine open: {e}"));
        }
    }
    if manifest.hierarchies.is_empty() {
        warnings.push("dataset has no routing/exact hierarchies".into());
    }

    Ok(VerificationReport {
        valid: errors.is_empty(),
        checked_files,
        errors,
        warnings,
    })
}

pub fn verify_dataset(root: impl AsRef<Path>) -> io::Result<VerificationReport> {
    let root = root.as_ref();
    let mut report = verify_dataset_structure(root)?;
    if !report.valid {
        return Ok(report);
    }

    let Some(seal) = read_integrity_manifest(root)? else {
        report
            .warnings
            .push("dataset is structurally valid but has no integrity seal".into());
        return Ok(report);
    };
    if seal.format != INTEGRITY_FORMAT {
        report.errors.push(format!("unsupported integrity format {}", seal.format));
        report.valid = false;
        return Ok(report);
    }

    let current = collect_stable_files(root)?;
    let current_set: BTreeSet<_> = current
        .iter()
        .map(|p| relative_string(root, p))
        .collect::<io::Result<_>>()?;
    let sealed_set: BTreeSet<_> = seal.entries.iter().map(|x| x.path.clone()).collect();
    for missing in sealed_set.difference(&current_set) {
        report.errors.push(format!("sealed file missing: {missing}"));
    }
    for extra in current_set.difference(&sealed_set) {
        report.errors.push(format!("unsealed stable file present: {extra}"));
    }

    for entry in &seal.entries {
        let path = root.join(&entry.path);
        if !path.is_file() {
            continue;
        }
        let (bytes, sha256) = sha256_file(&path)?;
        report.checked_files += 1;
        if bytes != entry.bytes {
            report.errors.push(format!(
                "size mismatch {}: {} != {}",
                entry.path, bytes, entry.bytes
            ));
        }
        if sha256 != entry.sha256 {
            report.errors.push(format!("checksum mismatch {}", entry.path));
        }
    }
    report.valid = report.errors.is_empty();
    Ok(report)
}

pub fn seal_dataset(root: impl AsRef<Path>) -> io::Result<IntegrityManifest> {
    let root = root.as_ref();
    let report = verify_dataset_structure(root)?;
    if !report.valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot seal invalid dataset: {}", report.errors.join("; ")),
        ));
    }
    let seal = compute_integrity(root)?;
    let data = serde_json::to_vec_pretty(&seal).map_err(json_error)?;
    atomic_write(&root.join("integrity.json"), &data)?;
    Ok(seal)
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let source = entry.path();
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if name_text == "temp" || name_text.ends_with(".tmp") || name_text.contains(".partial-") {
            continue;
        }
        let target = dst.join(name);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            copy_tree(&source, &target)?;
        } else if meta.is_file() {
            fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

pub fn backup_dataset(
    root: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> io::Result<VerificationReport> {
    let root = root.as_ref();
    let destination = destination.as_ref();
    let source_report = verify_dataset(root)?;
    if !source_report.valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("refusing to back up invalid dataset: {}", source_report.errors.join("; ")),
        ));
    }
    if destination.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "backup destination already exists",
        ));
    }
    if destination.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "backup destination cannot be inside dataset root",
        ));
    }

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let base = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let partial = parent.join(format!("{base}.partial-{}", std::process::id()));
    if partial.exists() {
        fs::remove_dir_all(&partial)?;
    }

    let result = (|| {
        copy_tree(root, &partial)?;
        let copied = verify_dataset(&partial)?;
        if !copied.valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("backup verification failed: {}", copied.errors.join("; ")),
            ));
        }
        fs::rename(&partial, destination)?;
        verify_dataset(destination)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&partial);
    }
    result
}

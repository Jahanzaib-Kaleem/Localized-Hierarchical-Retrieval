use crate::{abandon_generation, begin_generation, publish_generation, verify_dataset, GenerationInfo};
use std::{fs, io, path::Path};

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let source = entry.path();
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text == "temp" || text.ends_with(".tmp") || text.contains(".partial-") {
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

/// Verify a standalone generation backup, copy it into a writer-locked staging generation,
/// verify it again through normal publication, and atomically make it CURRENT.
///
/// Restore never overwrites an existing generation and therefore preserves rollback history.
pub fn restore_backup(
    catalog_root: impl AsRef<Path>,
    backup_root: impl AsRef<Path>,
) -> io::Result<GenerationInfo> {
    let catalog_root = catalog_root.as_ref();
    let backup_root = backup_root.as_ref();
    let source = verify_dataset(backup_root)?;
    if !source.valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("refusing to restore invalid backup: {}", source.errors.join("; ")),
        ));
    }

    let stage = begin_generation(catalog_root)?;
    let result = (|| {
        copy_tree(backup_root, &stage.path)?;
        let copied = verify_dataset(&stage.path)?;
        if !copied.valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("restored staging copy is invalid: {}", copied.errors.join("; ")),
            ));
        }
        Ok(())
    })();
    match result {
        Ok(()) => publish_generation(stage),
        Err(error) => {
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

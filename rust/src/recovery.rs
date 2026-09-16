use crate::{
    list_generations, rollback_generation, vacuum_generations, verify_dataset, GenerationInfo,
    VerificationReport, VersionedDataset,
};
use serde::Serialize;
use std::{io, path::Path};

#[derive(Debug, Clone, Serialize)]
pub struct RecoveryReport {
    pub current_generation: u64,
    pub changed_current: bool,
    pub stale_paths_removed: usize,
    pub verified_generations: usize,
}

/// Verify both the base physical generation and every overlay/delta layer reachable from it.
pub fn verify_versioned_dataset(root: impl AsRef<Path>) -> io::Result<VerificationReport> {
    let root = root.as_ref();
    let mut report = verify_dataset(root)?;
    if report.valid {
        if let Err(error) = VersionedDataset::open(root) {
            report.valid = false;
            report.errors.push(format!("versioned overlay: {error}"));
        }
    }
    Ok(report)
}

/// Recover a generation catalog after an interrupted writer or a damaged CURRENT generation.
/// The newest fully verified published generation becomes CURRENT; abandoned staging/work paths
/// are then removed without deleting any published generation.
pub fn recover_catalog(root: impl AsRef<Path>) -> io::Result<RecoveryReport> {
    let root = root.as_ref();
    let generations = list_generations(root)?;
    if generations.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "recovery requires at least one published generation",
        ));
    }

    let current = generations.iter().find(|x| x.current).map(|x| x.id);
    let mut verified = 0usize;
    let mut chosen: Option<GenerationInfo> = None;
    for generation in generations.iter().rev() {
        let report = verify_versioned_dataset(&generation.path)?;
        verified += 1;
        if report.valid {
            chosen = Some(generation.clone());
            break;
        }
    }
    let chosen = chosen.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "no published generation passes full versioned verification",
        )
    })?;

    let changed_current = current != Some(chosen.id);
    if changed_current {
        rollback_generation(root, chosen.id)?;
    }
    let cleanup = vacuum_generations(root, usize::MAX, &[])?;
    Ok(RecoveryReport {
        current_generation: chosen.id,
        changed_current,
        stale_paths_removed: cleanup.stale_paths_removed,
        verified_generations: verified,
    })
}

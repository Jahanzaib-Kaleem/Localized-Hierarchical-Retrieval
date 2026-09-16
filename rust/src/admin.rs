use crate::{
    abandon_generation, add_exact_hierarchies, begin_generation, dataset_status, publish_generation,
    read_schema, resolve_dataset_root, DatasetSchema, GenerationInfo, HierarchySpec, Manifest,
};
use serde::Serialize;
use std::{
    fs::{self, File},
    io::{self, Write},
    path::Path,
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ColumnStats {
    pub id: usize,
    pub name: String,
    pub cardinality: u64,
    pub nullable: bool,
    pub logical_type: String,
    pub dictionary_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IndexInfo {
    pub file: String,
    pub columns: Vec<usize>,
    pub column_names: Vec<String>,
    pub kind: String,
    pub entries: u64,
    pub keyspace: u64,
    pub bytes: u64,
    pub exact_rows: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DatasetStatsReport {
    pub rows: u64,
    pub columns: usize,
    pub pages: u32,
    pub canonical_bytes: u64,
    pub routing_bytes: u64,
    pub total_bytes: u64,
    pub column_stats: Vec<ColumnStats>,
    pub indexes: Vec<IndexInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IndexChangeReport {
    pub generation: GenerationInfo,
    pub action: String,
    pub columns: Vec<String>,
    pub indexes: Vec<IndexInfo>,
}

fn exact_row_kind(kind: &str) -> bool {
    matches!(
        kind,
        "postings" | "densepost" | "deltapost" | "flatpost" | "bitslice"
    )
}

fn logical_type_name(column: &crate::ColumnSchema) -> String {
    format!("{:?}", column.logical_type).to_ascii_lowercase()
}

fn read_manifest(root: &Path) -> io::Result<Manifest> {
    serde_json::from_slice(&fs::read(root.join("manifest.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn index_info(root: &Path, schema: &DatasetSchema, manifest: &Manifest) -> Vec<IndexInfo> {
    manifest
        .hierarchies
        .iter()
        .map(|hierarchy| IndexInfo {
            file: hierarchy.file.clone(),
            columns: hierarchy.columns.clone(),
            column_names: hierarchy
                .columns
                .iter()
                .map(|&column| {
                    schema
                        .columns
                        .get(column)
                        .map(|x| x.name.clone())
                        .unwrap_or_else(|| format!("#{column}"))
                })
                .collect(),
            kind: hierarchy.kind.clone(),
            entries: hierarchy.entries,
            keyspace: hierarchy.keyspace,
            bytes: fs::metadata(root.join("routing").join(&hierarchy.file))
                .map(|x| x.len())
                .unwrap_or(0),
            exact_rows: exact_row_kind(&hierarchy.kind),
        })
        .collect()
}

pub fn dataset_stats(root: impl AsRef<Path>) -> io::Result<DatasetStatsReport> {
    let root = resolve_dataset_root(root)?;
    let schema = read_schema(&root)?;
    let manifest = read_manifest(&root)?;
    let status = dataset_status(&root)?;
    let column_stats = schema
        .columns
        .iter()
        .enumerate()
        .map(|(id, column)| ColumnStats {
            id,
            name: column.name.clone(),
            cardinality: manifest.cardinalities.get(id).copied().unwrap_or(0),
            nullable: column.nullable,
            logical_type: logical_type_name(column),
            dictionary_bytes: fs::metadata(
                root.join("dictionaries")
                    .join(crate::dictionary_filename(id)),
            )
            .map(|x| x.len())
            .unwrap_or(0),
        })
        .collect();
    Ok(DatasetStatsReport {
        rows: manifest.rows,
        columns: manifest.columns,
        pages: manifest.pages,
        canonical_bytes: status.canonical_bytes,
        routing_bytes: status.routing_bytes,
        total_bytes: status.total_bytes,
        column_stats,
        indexes: index_info(&root, &schema, &manifest),
    })
}

pub fn list_indexes(root: impl AsRef<Path>) -> io::Result<Vec<IndexInfo>> {
    let root = resolve_dataset_root(root)?;
    let schema = read_schema(&root)?;
    let manifest = read_manifest(&root)?;
    Ok(index_info(&root, &schema, &manifest))
}

fn columns_from_names(schema: &DatasetSchema, names: &[String]) -> io::Result<Vec<usize>> {
    if names.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "accelerator indexes require at least two columns",
        ));
    }
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let column = schema.column_index(name).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("unknown column {name}"))
        })?;
        columns.push(column);
    }
    columns.sort_unstable();
    let before = columns.len();
    columns.dedup();
    if columns.len() != before {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "index columns must be unique",
        ));
    }
    Ok(columns)
}

fn clone_tree_link(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let source = entry.path();
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text == "temp"
            || text == "integrity.json"
            || text.ends_with(".tmp")
            || text.contains(".partial-")
        {
            continue;
        }
        let target = dst.join(name);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            clone_tree_link(&source, &target)?;
        } else if meta.is_file() {
            if fs::hard_link(&source, &target).is_err() {
                fs::copy(&source, &target)?;
            }
        }
    }
    Ok(())
}

fn atomic_manifest(root: &Path, manifest: &Manifest) -> io::Result<()> {
    let tmp = root.join(format!(".manifest.json.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(
            &serde_json::to_vec_pretty(manifest)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        )?;
        file.sync_all()?;
        fs::rename(&tmp, root.join("manifest.json"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn clone_current(
    catalog_root: &Path,
) -> io::Result<(crate::StagedGeneration, DatasetSchema, Manifest)> {
    let stage = begin_generation(catalog_root)?;
    let result = (|| {
        let current = resolve_dataset_root(catalog_root)?;
        let schema = read_schema(&current)?;
        let manifest = read_manifest(&current)?;
        clone_tree_link(&current, &stage.path)?;
        Ok((schema, manifest))
    })();
    match result {
        Ok((schema, manifest)) => Ok((stage, schema, manifest)),
        Err(error) => {
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

fn report_after(
    generation: GenerationInfo,
    action: &str,
    columns: &[String],
) -> io::Result<IndexChangeReport> {
    let indexes = list_indexes(&generation.path)?;
    Ok(IndexChangeReport {
        generation,
        action: action.into(),
        columns: columns.to_vec(),
        indexes,
    })
}

pub fn add_index(
    catalog_root: impl AsRef<Path>,
    column_names: &[String],
    max_sort_records: usize,
) -> io::Result<IndexChangeReport> {
    if max_sort_records == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "max_sort_records must be > 0",
        ));
    }
    let catalog_root = catalog_root.as_ref();
    let (stage, schema, _) = clone_current(catalog_root)?;
    let columns = columns_from_names(&schema, column_names)?;
    let result = add_exact_hierarchies(
        &stage.path,
        &[HierarchySpec { columns }],
        max_sort_records,
    );
    match result {
        Ok(_) => {
            let generation = publish_generation(stage)?;
            report_after(generation, "add", column_names)
        }
        Err(error) => {
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

fn drop_from_stage(
    stage: &crate::StagedGeneration,
    manifest: &mut Manifest,
    columns: &[usize],
) -> io::Result<()> {
    let matches: Vec<_> = manifest
        .hierarchies
        .iter()
        .enumerate()
        .filter(|(_, hierarchy)| exact_row_kind(&hierarchy.kind) && hierarchy.columns == columns)
        .map(|(index, hierarchy)| (index, hierarchy.file.clone()))
        .collect();
    if matches.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "exact accelerator does not exist",
        ));
    }
    for (_, file) in &matches {
        fs::remove_file(stage.path.join("routing").join(file))?;
    }
    let remove: std::collections::BTreeSet<_> = matches.iter().map(|(index, _)| *index).collect();
    manifest.hierarchies = manifest
        .hierarchies
        .iter()
        .enumerate()
        .filter(|(index, _)| !remove.contains(index))
        .map(|(_, hierarchy)| hierarchy.clone())
        .collect();
    atomic_manifest(&stage.path, manifest)
}

pub fn drop_index(
    catalog_root: impl AsRef<Path>,
    column_names: &[String],
) -> io::Result<IndexChangeReport> {
    let catalog_root = catalog_root.as_ref();
    let (stage, schema, mut manifest) = clone_current(catalog_root)?;
    let columns = columns_from_names(&schema, column_names)?;
    let result = drop_from_stage(&stage, &mut manifest, &columns);
    match result {
        Ok(()) => {
            let generation = publish_generation(stage)?;
            report_after(generation, "drop", column_names)
        }
        Err(error) => {
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

pub fn rebuild_index(
    catalog_root: impl AsRef<Path>,
    column_names: &[String],
    max_sort_records: usize,
) -> io::Result<IndexChangeReport> {
    if max_sort_records == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "max_sort_records must be > 0",
        ));
    }
    let catalog_root = catalog_root.as_ref();
    let (stage, schema, mut manifest) = clone_current(catalog_root)?;
    let columns = columns_from_names(&schema, column_names)?;
    let result = (|| {
        drop_from_stage(&stage, &mut manifest, &columns)?;
        add_exact_hierarchies(
            &stage.path,
            &[HierarchySpec {
                columns: columns.clone(),
            }],
            max_sort_records,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            let generation = publish_generation(stage)?;
            report_after(generation, "rebuild", column_names)
        }
        Err(error) => {
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

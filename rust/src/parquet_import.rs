use crate::{
    abandon_generation, add_exact_hierarchies, begin_generation, dictionary_filename,
    publish_generation, resolve_dataset_root, write_schema, BuildConfig, DatasetSchema, Dictionary,
    GenerationInfo, HierarchySpec,
};
use crate::dictionary_build::DictionarySpool;
use crate::stream_builder::U32StreamBuilder;
use parquet::{
    file::reader::{FileReader, SerializedFileReader},
    record::Field,
};
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashMap},
    fs::File,
    io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct ParquetImportConfig {
    pub page_rows: usize,
    pub batch_rows: usize,
    pub max_sort_records: usize,
    pub dictionary_run_bytes: usize,
    pub accelerators: Vec<Vec<usize>>,
    pub cold_columns: Vec<usize>,
    pub row_group_rows: usize,
}

impl Default for ParquetImportConfig {
    fn default() -> Self {
        Self {
            page_rows: 1024,
            batch_rows: 16_384,
            max_sort_records: 250_000,
            dictionary_run_bytes: 16 * 1024 * 1024,
            accelerators: Vec::new(),
            cold_columns: Vec::new(),
            row_group_rows: 65_536,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParquetImportReport {
    pub generation: GenerationInfo,
    pub source_files: usize,
    pub rows: u64,
    pub cardinalities: Vec<u64>,
    pub exact_hierarchies: usize,
    pub hot_columns: Vec<usize>,
    pub cold_columns: Vec<usize>,
    pub row_group_rows: usize,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn data_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn parquet_error(context: &str, error: impl std::fmt::Display) -> io::Error {
    data_error(format!("{context}: {error}"))
}

fn contextual(error: io::Error, path: &Path, row: u64, column: &str) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "Parquet {}, row {}, column {}: {}",
            path.display(),
            row + 1,
            column,
            error
        ),
    )
}

fn validate_config(config: &ParquetImportConfig, columns: usize) -> io::Result<Vec<usize>> {
    if config.page_rows == 0
        || config.batch_rows == 0
        || config.max_sort_records == 0
        || config.dictionary_run_bytes == 0
        || config.row_group_rows == 0
    {
        return Err(invalid(
            "page_rows, batch_rows, max_sort_records, dictionary_run_bytes, and row_group_rows must be > 0",
        ));
    }
    if config.cold_columns.is_empty() {
        return Err(invalid(
            "direct Parquet import requires an explicit hybrid profile with at least one cold column",
        ));
    }
    let mut cold = config.cold_columns.clone();
    cold.sort_unstable();
    cold.dedup();
    if cold != config.cold_columns || cold.iter().any(|&column| column >= columns) {
        return Err(invalid("cold columns must be sorted, unique, and in range"));
    }
    let hot = (0..columns)
        .filter(|column| cold.binary_search(column).is_err())
        .collect::<Vec<_>>();
    if hot.is_empty() {
        return Err(invalid("hybrid profile must retain at least one hot column"));
    }
    Ok(hot)
}

fn validate_sources(sources: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    if sources.is_empty() {
        return Err(invalid("at least one Parquet source file is required"));
    }
    let mut ordered = sources.to_vec();
    ordered.sort();
    ordered.dedup();
    if ordered.len() != sources.len() {
        return Err(invalid("duplicate Parquet source path"));
    }
    for path in &ordered {
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Parquet source is not a file: {}", path.display()),
            ));
        }
    }
    Ok(ordered)
}

fn field_value(field: &Field, column: &str) -> io::Result<Option<String>> {
    match field {
        Field::Null => Ok(None),
        Field::Str(value) => Ok(Some(value.clone())),
        Field::Bytes(value) => std::str::from_utf8(value.data())
            .map(|value| Some(value.to_owned()))
            .map_err(|error| {
                data_error(format!(
                    "binary value in text column {column:?} is not UTF-8: {error}"
                ))
            }),
        value if value.is_primitive() => Ok(Some(format!("{value}"))),
        _ => Err(data_error(format!(
            "nested Parquet value in column {column:?} is not supported by LHR-SCHEMA/1; flatten it before import"
        ))),
    }
}

fn source_positions(
    reader: &SerializedFileReader<File>,
    schema: &DatasetSchema,
    path: &Path,
) -> io::Result<Vec<usize>> {
    let descr = reader.metadata().file_metadata().schema_descr();
    let mut positions = HashMap::<String, usize>::new();
    for index in 0..descr.num_columns() {
        let name = descr.column(index).name().to_owned();
        if positions.insert(name.clone(), index).is_some() {
            return Err(data_error(format!(
                "Parquet {} has duplicate leaf column name {name:?}; nested/ambiguous schemas must be flattened first",
                path.display()
            )));
        }
    }
    if positions.len() != schema.columns.len() {
        return Err(data_error(format!(
            "Parquet {} exposes {} leaf columns but LHR schema declares {}",
            path.display(),
            positions.len(),
            schema.columns.len()
        )));
    }
    schema
        .columns
        .iter()
        .map(|column| {
            positions.get(&column.name).copied().ok_or_else(|| {
                data_error(format!(
                    "Parquet {} is missing required column {:?}",
                    path.display(),
                    column.name
                ))
            })
        })
        .collect()
}

fn scan_rows<F>(sources: &[PathBuf], schema: &DatasetSchema, mut callback: F) -> io::Result<u64>
where
    F: FnMut(&Path, u64, &[Option<String>]) -> io::Result<()>,
{
    let mut total = 0u64;
    for path in sources {
        let reader = SerializedFileReader::new(File::open(path)?)
            .map_err(|error| parquet_error("open source Parquet", error))?;
        let positions = source_positions(&reader, schema, path)?;
        let expected_rows = u64::try_from(reader.metadata().file_metadata().num_rows())
            .map_err(|_| data_error("Parquet metadata contains a negative row count"))?;
        let mut shard_rows = 0u64;
        let iter = reader
            .get_row_iter(None)
            .map_err(|error| parquet_error("create Parquet row iterator", error))?;
        for row in iter {
            let row = row.map_err(|error| parquet_error("decode Parquet row", error))?;
            let fields = row.get_column_iter().collect::<Vec<_>>();
            if fields.len() != positions.len() {
                return Err(data_error(format!(
                    "Parquet {} row API exposes {} top-level fields but {} flat columns were expected; nested schemas must be flattened first",
                    path.display(),
                    fields.len(),
                    positions.len()
                )));
            }
            let mut values = Vec::with_capacity(schema.columns.len());
            for (column_index, &source_index) in positions.iter().enumerate() {
                let (_, field) = fields.get(source_index).ok_or_else(|| {
                    data_error(format!(
                        "Parquet {} row is missing source column slot {}",
                        path.display(),
                        source_index
                    ))
                })?;
                values.push(field_value(field, &schema.columns[column_index].name)?);
            }
            callback(path, shard_rows, &values)?;
            shard_rows = shard_rows
                .checked_add(1)
                .ok_or_else(|| data_error("Parquet shard row count overflow"))?;
            total = total
                .checked_add(1)
                .ok_or_else(|| data_error("Parquet total row count overflow"))?;
        }
        if shard_rows != expected_rows {
            return Err(data_error(format!(
                "Parquet {} metadata reports {expected_rows} rows but row iterator produced {shard_rows}",
                path.display()
            )));
        }
    }
    Ok(total)
}

fn exact_specs(columns: usize, accelerators: &[Vec<usize>]) -> io::Result<Vec<HierarchySpec>> {
    let mut set = BTreeSet::<Vec<usize>>::new();
    for column in 0..columns {
        set.insert(vec![column]);
    }
    for input in accelerators {
        if input.len() < 2 {
            return Err(invalid("accelerators must contain at least two columns"));
        }
        let mut spec = input.clone();
        spec.sort_unstable();
        spec.dedup();
        if spec.len() != input.len() || spec.iter().any(|&column| column >= columns) {
            return Err(invalid("accelerator contains duplicate or invalid column"));
        }
        set.insert(spec);
    }
    Ok(set
        .into_iter()
        .map(|columns| HierarchySpec { columns })
        .collect())
}

/// Import one or more existing Parquet shards directly into a new immutable LHR generation.
///
/// The source Parquet files are only read. No CSV or decoded full-dataset staging file is created.
/// Pass 1 builds LHR dictionaries; pass 2 streams dictionary tokens directly into the hybrid
/// canonical writer. The sources remain untouched at their original paths.
pub fn import_parquet_shards_initial(
    catalog_root: impl AsRef<Path>,
    sources: &[PathBuf],
    schema: &DatasetSchema,
    config: &ParquetImportConfig,
) -> io::Result<ParquetImportReport> {
    schema.validate()?;
    let catalog_root = catalog_root.as_ref();
    match resolve_dataset_root(catalog_root) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "initial Parquet import requires an empty LHR catalog",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let sources = validate_sources(sources)?;
    let hot_columns = validate_config(config, schema.columns.len())?;
    let specs = exact_specs(schema.columns.len(), &config.accelerators)?;
    let stage = begin_generation(catalog_root)?;

    let result = (|| {
        let mut spool = DictionarySpool::create(&stage.path, schema.columns.len())?;
        let first_rows = scan_rows(&sources, schema, |path, row, values| {
            for (column_index, value) in values.iter().enumerate() {
                let column = &schema.columns[column_index];
                match value {
                    None if column.nullable => {}
                    None => {
                        return Err(contextual(
                            data_error("NULL value in non-nullable column"),
                            path,
                            row,
                            &column.name,
                        ))
                    }
                    Some(raw) => {
                        let canonical = column
                            .canonicalize(raw)
                            .map_err(|error| contextual(error, path, row, &column.name))?;
                        spool.push(column_index, &canonical)?;
                    }
                }
            }
            Ok(())
        })?;
        if first_rows == 0 {
            return Err(data_error("Parquet sources contain no rows"));
        }
        let cardinalities = spool.finish(schema, config.dictionary_run_bytes)?;
        let dictionaries = (0..schema.columns.len())
            .map(|column| {
                Dictionary::open(stage.path.join("dictionaries").join(dictionary_filename(column)))
            })
            .collect::<io::Result<Vec<_>>>()?;

        let build_cfg = BuildConfig {
            columns: schema.columns.len(),
            page_rows: config.page_rows,
            cardinalities: cardinalities.clone(),
            hierarchies: Vec::new(),
            max_sort_records: config.max_sort_records,
        };
        let mut builder = U32StreamBuilder::create(
            &stage.path,
            build_cfg,
            Some(config.cold_columns.clone()),
            config.row_group_rows,
        )?;
        let columns = schema.columns.len();
        let mut batch = Vec::<u32>::with_capacity(config.batch_rows.saturating_mul(columns));
        let second_rows = scan_rows(&sources, schema, |path, row, values| {
            for (column_index, value) in values.iter().enumerate() {
                let column = &schema.columns[column_index];
                let token = match value {
                    None if column.nullable => 0,
                    None => {
                        return Err(contextual(
                            data_error("NULL value in non-nullable column"),
                            path,
                            row,
                            &column.name,
                        ))
                    }
                    Some(raw) => {
                        let canonical = column
                            .canonicalize(raw)
                            .map_err(|error| contextual(error, path, row, &column.name))?;
                        dictionaries[column_index].token(&canonical).ok_or_else(|| {
                            contextual(
                                data_error("value is absent from first-pass dictionary"),
                                path,
                                row,
                                &column.name,
                            )
                        })?
                    }
                };
                batch.push(token);
            }
            if batch.len() / columns >= config.batch_rows {
                builder.push_batch(std::mem::take(&mut batch))?;
                batch = Vec::with_capacity(config.batch_rows.saturating_mul(columns));
            }
            Ok(())
        })?;
        if !batch.is_empty() {
            builder.push_batch(batch)?;
        }
        if second_rows != first_rows {
            return Err(data_error(format!(
                "Parquet sources changed between dictionary and token passes: first={first_rows}, second={second_rows}"
            )));
        }
        let manifest = builder.finish()?;
        if manifest.rows != first_rows {
            return Err(data_error(format!(
                "stream builder wrote {} rows, expected {first_rows}",
                manifest.rows
            )));
        }
        add_exact_hierarchies(&stage.path, &specs, config.max_sort_records)?;
        write_schema(&stage.path, schema)?;
        Ok((first_rows, cardinalities))
    })();

    match result {
        Ok((rows, cardinalities)) => {
            let generation = publish_generation(stage)?;
            Ok(ParquetImportReport {
                generation,
                source_files: sources.len(),
                rows,
                cardinalities,
                exact_hierarchies: specs.len(),
                hot_columns,
                cold_columns: config.cold_columns.clone(),
                row_group_rows: config.row_group_rows,
            })
        }
        Err(error) => {
            let _ = abandon_generation(stage);
            Err(error)
        }
    }
}

use clap::Parser;
use lhr::{
    import_parquet_shards_initial, read_schema_file, verify_versioned_dataset, DatasetSchema,
    ParquetImportConfig,
};
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeSet, error::Error, fs, path::PathBuf, process};

#[derive(Parser, Debug)]
#[command(
    name = "lhr-parquet-import",
    version,
    about = "Direct multi-shard Parquet -> LHR hybrid importer (no CSV staging)"
)]
struct Cli {
    #[arg(long, default_value = ".")]
    root: PathBuf,

    #[arg(long)]
    schema: PathBuf,

    /// Source Parquet shards. Shell globs are supported by normal shell expansion.
    #[arg(value_name = "PARQUET", required = true)]
    sources: Vec<PathBuf>,

    /// Columns to keep in mmap-native LHR storage. Every other column becomes cold.
    #[arg(long = "hot")]
    hot_columns: Vec<String>,

    /// Columns to keep in Parquet token sidecars. Every other column remains hot.
    #[arg(long = "cold")]
    cold_columns: Vec<String>,

    /// Optional JSON profile with `hot_columns` or `cold_columns`, plus optional row_group_rows.
    #[arg(long)]
    profile: Option<PathBuf>,

    /// Additional exact multi-column accelerator, e.g. --index country,title
    #[arg(long = "index")]
    indexes: Vec<String>,

    #[arg(long, default_value_t = 1024)]
    page_rows: usize,

    #[arg(long, default_value_t = 16_384)]
    batch_rows: usize,

    #[arg(long, default_value_t = 250_000)]
    max_sort_records: usize,

    #[arg(long, default_value_t = 16_777_216)]
    dictionary_run_bytes: usize,

    #[arg(long, default_value_t = 65_536)]
    row_group_rows: usize,

    /// Skip the post-publish structural/integrity verification.
    #[arg(long)]
    no_verify: bool,
}

#[derive(Debug, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    hot_columns: Vec<String>,
    #[serde(default)]
    cold_columns: Vec<String>,
    row_group_rows: Option<usize>,
}

fn column_ids(names: &[String], schema: &DatasetSchema) -> Result<Vec<usize>, Box<dyn Error>> {
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        ids.push(
            schema
                .column_index(name)
                .ok_or_else(|| format!("unknown column {name:?}"))?,
        );
    }
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != names.len() {
        return Err("hot/cold column lists may not contain duplicates".into());
    }
    Ok(ids)
}

fn cold_profile(
    schema: &DatasetSchema,
    hot: &[String],
    cold: &[String],
) -> Result<Vec<usize>, Box<dyn Error>> {
    if !hot.is_empty() && !cold.is_empty() {
        return Err("choose either hot columns or cold columns, not both".into());
    }
    if hot.is_empty() && cold.is_empty() {
        return Err(
            "hybrid import requires an explicit profile: provide --hot, --cold, or --profile"
                .into(),
        );
    }
    if !cold.is_empty() {
        let cold = column_ids(cold, schema)?;
        if cold.len() == schema.columns.len() {
            return Err("at least one column must remain hot".into());
        }
        return Ok(cold);
    }
    let hot = column_ids(hot, schema)?.into_iter().collect::<BTreeSet<_>>();
    if hot.is_empty() {
        return Err("at least one column must remain hot".into());
    }
    let cold = (0..schema.columns.len())
        .filter(|column| !hot.contains(column))
        .collect::<Vec<_>>();
    if cold.is_empty() {
        return Err("at least one column must be cold for hybrid Parquet storage".into());
    }
    Ok(cold)
}

fn parse_indexes(raw: &[String], schema: &DatasetSchema) -> Result<Vec<Vec<usize>>, Box<dyn Error>> {
    raw.iter()
        .map(|index| {
            let names = index
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>();
            if names.len() < 2 {
                return Err(format!(
                    "index {index:?} must contain at least two comma-separated columns"
                )
                .into());
            }
            let mut columns = names
                .iter()
                .map(|name| {
                    schema
                        .column_index(name)
                        .ok_or_else(|| format!("unknown index column {name:?}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let before = columns.len();
            columns.sort_unstable();
            columns.dedup();
            if columns.len() != before {
                return Err(format!("index {index:?} contains duplicate columns").into());
            }
            Ok(columns)
        })
        .collect()
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let schema = read_schema_file(&cli.schema)?;

    let mut hot = cli.hot_columns;
    let mut cold = cli.cold_columns;
    let mut row_group_rows = cli.row_group_rows;
    if let Some(profile_path) = cli.profile {
        if !hot.is_empty() || !cold.is_empty() {
            return Err("--profile cannot be combined with --hot/--cold".into());
        }
        let profile: ProfileFile = serde_json::from_slice(&fs::read(profile_path)?)?;
        hot = profile.hot_columns;
        cold = profile.cold_columns;
        if let Some(value) = profile.row_group_rows {
            row_group_rows = value;
        }
    }

    let cold_columns = cold_profile(&schema, &hot, &cold)?;
    let config = ParquetImportConfig {
        page_rows: cli.page_rows,
        batch_rows: cli.batch_rows,
        max_sort_records: cli.max_sort_records,
        dictionary_run_bytes: cli.dictionary_run_bytes,
        accelerators: parse_indexes(&cli.indexes, &schema)?,
        cold_columns,
        row_group_rows,
    };
    let report = import_parquet_shards_initial(&cli.root, &cli.sources, &schema, &config)?;
    if cli.no_verify {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let verification = verify_versioned_dataset(&report.generation.path)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "import": report,
            "verification": verification,
        }))?
    );
    if !verification.valid {
        process::exit(2);
    }
    Ok(())
}

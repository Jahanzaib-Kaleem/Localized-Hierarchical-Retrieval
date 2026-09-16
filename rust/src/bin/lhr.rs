use clap::{Parser, Subcommand};
use lhr::{
    apply_mutations, backup_dataset, dataset_status, import_csv, list_generations,
    read_schema_file, resolve_dataset_root, rollback_generation, seal_dataset, verify_dataset,
    CsvImportConfig, DatasetSchema, Engine, LogicalDataset, LogicalPredicate, Mutation,
    MutationConfig, Predicate,
};
use serde_json::json;
use std::{error::Error, fs, path::PathBuf, process};

#[derive(Parser, Debug)]
#[command(name = "lhr", version, about = "Localized Hierarchical Retrieval operational CLI")]
struct Cli {
    /// LHR dataset or generation-catalog root.
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show dataset metadata and storage usage.
    Status,
    /// Verify manifest/segment/index structure and integrity seal when present.
    Verify,
    /// Compute SHA-256 checksums for all stable files in the current generation.
    Seal,
    /// Run an exact encoded-token query. Predicates use COLUMN=VALUE, e.g. 2=17.
    Query {
        #[arg(value_name = "COLUMN=VALUE", required = true)]
        predicates: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Query using schema column names and original external values.
    QueryValues {
        #[arg(value_name = "NAME=VALUE")]
        predicates: Vec<String>,
        /// Add an equality predicate matching NULL for this column. Repeatable.
        #[arg(long = "is-null")]
        null_columns: Vec<String>,
        /// Return only these named columns. Repeatable; defaults to all columns.
        #[arg(long)]
        select: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Import external data into a new immutable generation.
    Import {
        #[command(subcommand)]
        command: ImportCommand,
    },
    /// Apply a JSON array of insert/update/delete operations as one atomic generation.
    Mutate {
        file: PathBuf,
        #[arg(long, default_value_t = 16_384)]
        batch_rows: usize,
        #[arg(long, default_value_t = 250_000)]
        max_sort_records: usize,
        #[arg(long, default_value_t = 67_108_864)]
        dictionary_run_bytes: usize,
    },
    /// Back up the current resolved generation to a new destination.
    Backup {
        destination: PathBuf,
    },
    /// Inspect or repoint immutable dataset generations.
    Generations {
        #[command(subcommand)]
        command: GenerationCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ImportCommand {
    /// Two-pass bounded-memory CSV import using a schema JSON file.
    Csv {
        source: PathBuf,
        #[arg(long)]
        schema: PathBuf,
        /// Additional exact accelerator, written as comma-separated schema column names.
        /// Repeatable. Single-column exact indexes are always built automatically.
        #[arg(long = "index")]
        indexes: Vec<String>,
        #[arg(long, default_value_t = 1024)]
        page_rows: usize,
        #[arg(long, default_value_t = 16_384)]
        batch_rows: usize,
        #[arg(long, default_value_t = 250_000)]
        max_sort_records: usize,
        #[arg(long, default_value_t = 67_108_864)]
        dictionary_run_bytes: usize,
    },
}

#[derive(Subcommand, Debug)]
enum GenerationCommand {
    /// List all published generations and identify CURRENT.
    List,
    /// Print the path currently resolved for queries.
    Current,
    /// Atomically repoint CURRENT to an existing verified generation.
    Rollback { id: u64 },
}

fn parse_predicate(raw: &str) -> Result<Predicate, Box<dyn Error>> {
    let (column, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid predicate {raw:?}; expected COLUMN=VALUE"))?;
    Ok(Predicate {
        column: column.parse()?,
        value: value.parse()?,
    })
}

fn parse_logical_predicate(raw: &str) -> Result<LogicalPredicate, Box<dyn Error>> {
    let (column, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid predicate {raw:?}; expected NAME=VALUE"))?;
    if column.is_empty() {
        return Err("logical predicate column cannot be empty".into());
    }
    Ok(LogicalPredicate {
        column: column.to_owned(),
        value: Some(value.to_owned()),
    })
}

fn parse_indexes(indexes: &[String], schema: &DatasetSchema) -> Result<Vec<Vec<usize>>, Box<dyn Error>> {
    indexes
        .iter()
        .map(|raw| {
            let names: Vec<_> = raw
                .split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .collect();
            if names.len() < 2 {
                return Err(format!("index {raw:?} must contain at least two column names").into());
            }
            names
                .into_iter()
                .map(|name| {
                    schema
                        .column_index(name)
                        .ok_or_else(|| format!("unknown index column {name:?}").into())
                })
                .collect::<Result<Vec<_>, Box<dyn Error>>>()
        })
        .collect()
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), Box<dyn Error>> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Import { command } => match command {
            ImportCommand::Csv {
                source,
                schema,
                indexes,
                page_rows,
                batch_rows,
                max_sort_records,
                dictionary_run_bytes,
            } => {
                let schema = read_schema_file(schema)?;
                let config = CsvImportConfig {
                    page_rows,
                    batch_rows,
                    max_sort_records,
                    dictionary_run_bytes,
                    accelerators: parse_indexes(&indexes, &schema)?,
                };
                print_json(&import_csv(&cli.root, source, &schema, &config)?)?;
            }
        },
        Command::Mutate {
            file,
            batch_rows,
            max_sort_records,
            dictionary_run_bytes,
        } => {
            let mutations: Vec<Mutation> = serde_json::from_slice(&fs::read(file)?)?;
            let config = MutationConfig {
                batch_rows,
                max_sort_records,
                dictionary_run_bytes,
            };
            print_json(&apply_mutations(&cli.root, &mutations, &config)?)?;
        }
        Command::Generations { command } => match command {
            GenerationCommand::List => print_json(&list_generations(&cli.root)?)?,
            GenerationCommand::Current => {
                let resolved = resolve_dataset_root(&cli.root)?;
                print_json(&json!({ "path": resolved }))?;
            }
            GenerationCommand::Rollback { id } => {
                print_json(&rollback_generation(&cli.root, id)?)?;
            }
        },
        command => {
            let dataset = resolve_dataset_root(&cli.root)?;
            match command {
                Command::Status => print_json(&dataset_status(&dataset)?)?,
                Command::Verify => {
                    let report = verify_dataset(&dataset)?;
                    print_json(&report)?;
                    if !report.valid {
                        process::exit(2);
                    }
                }
                Command::Seal => {
                    let seal = seal_dataset(&dataset)?;
                    let report = verify_dataset(&dataset)?;
                    print_json(&json!({
                        "sealed_files": seal.entries.len(),
                        "generation_path": dataset,
                        "verification": report,
                    }))?;
                    if !report.valid {
                        process::exit(2);
                    }
                }
                Command::Query { predicates, limit } => {
                    let predicates = predicates
                        .iter()
                        .map(|x| parse_predicate(x))
                        .collect::<Result<Vec<_>, _>>()?;
                    let engine = Engine::open(&dataset)?;
                    let (row_ids, stats) = engine.query_row_ids(&predicates, limit);
                    print_json(&json!({
                        "generation_path": dataset,
                        "physical_row_ids": row_ids,
                        "returned": row_ids.len(),
                        "limit": limit,
                        "stats": {
                            "hits": stats.hits,
                            "rows_checked": stats.rows_checked,
                            "pages_touched": stats.pages_touched,
                            "hierarchy_lookups": stats.hierarchy_lookups,
                        }
                    }))?;
                }
                Command::QueryValues {
                    predicates,
                    null_columns,
                    select,
                    limit,
                } => {
                    let mut predicates = predicates
                        .iter()
                        .map(|x| parse_logical_predicate(x))
                        .collect::<Result<Vec<_>, _>>()?;
                    predicates.extend(null_columns.into_iter().map(|column| LogicalPredicate {
                        column,
                        value: None,
                    }));
                    if predicates.is_empty() {
                        return Err("query-values requires at least one predicate".into());
                    }
                    let logical = LogicalDataset::open(&cli.root)?;
                    let selection = if select.is_empty() {
                        None
                    } else {
                        Some(select.as_slice())
                    };
                    print_json(&logical.query_values(&predicates, selection, limit)?)?;
                }
                Command::Backup { destination } => {
                    let report = backup_dataset(&dataset, &destination)?;
                    print_json(&json!({
                        "source_generation": dataset,
                        "destination": destination,
                        "verification": report,
                    }))?;
                }
                Command::Import { .. } | Command::Mutate { .. } | Command::Generations { .. } => {
                    unreachable!()
                }
            }
        }
    }
    Ok(())
}

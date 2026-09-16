use clap::{Parser, Subcommand};
use lhr::{
    add_index, apply_mutations_delta, backup_dataset, compact_dataset, dataset_stats,
    dataset_status, drop_index, import_csv, list_generations, list_indexes, read_schema_file,
    rebuild_index, resolve_dataset_root, restore_backup, rollback_generation, seal_dataset,
    vacuum_generations, verify_dataset, CompactionConfig, CsvImportConfig, DatasetSchema, Engine,
    LogicalPredicate, Mutation, MutationConfig, Predicate, VersionedDataset,
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
    /// Show compact dataset metadata and storage usage.
    Status,
    /// Show schema cardinalities, dictionary sizes, and exact/routing index details.
    Stats,
    /// Verify manifest/segment/index structure and integrity seal when present.
    Verify,
    /// Compute SHA-256 checksums for all stable files in the current generation.
    Seal,
    /// Run an exact encoded-token query against the base physical generation.
    Query {
        #[arg(value_name = "COLUMN=VALUE", required = true)]
        predicates: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Explain the exact encoded-token query plan for the base physical generation.
    Explain {
        #[arg(value_name = "COLUMN=VALUE", required = true)]
        predicates: Vec<String>,
    },
    /// Query the latest visible logical row versions using schema names and external values.
    QueryValues {
        #[arg(value_name = "NAME=VALUE")]
        predicates: Vec<String>,
        #[arg(long = "is-null")]
        null_columns: Vec<String>,
        #[arg(long)]
        select: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Explain named-value plans for the base and every immutable delta layer.
    ExplainValues {
        #[arg(value_name = "NAME=VALUE")]
        predicates: Vec<String>,
        #[arg(long = "is-null")]
        null_columns: Vec<String>,
    },
    /// Import external data into a new immutable generation.
    Import {
        #[command(subcommand)]
        command: ImportCommand,
    },
    /// Apply insert/update/delete operations as one append-only immutable delta transaction.
    Mutate {
        file: PathBuf,
        #[arg(long, default_value_t = 16_384)]
        batch_rows: usize,
        #[arg(long, default_value_t = 250_000)]
        max_sort_records: usize,
        #[arg(long, default_value_t = 67_108_864)]
        dictionary_run_bytes: usize,
    },
    /// Merge all visible row versions into a clean base generation and discard delta history.
    Compact {
        #[arg(long, default_value_t = 16_384)]
        batch_rows: usize,
        #[arg(long, default_value_t = 250_000)]
        max_sort_records: usize,
        #[arg(long, default_value_t = 67_108_864)]
        dictionary_run_bytes: usize,
    },
    /// Add, drop, rebuild, or inspect exact accelerator indexes.
    Indexes {
        #[command(subcommand)]
        command: IndexCommand,
    },
    /// Back up the current resolved generation to a new destination.
    Backup {
        destination: PathBuf,
    },
    /// Restore a verified standalone generation backup as a new CURRENT generation.
    Restore {
        backup: PathBuf,
    },
    /// Inspect, repoint, or prune immutable dataset generations.
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
enum IndexCommand {
    List,
    Add {
        #[arg(value_name = "COLUMN", required = true, num_args = 2..)]
        columns: Vec<String>,
        #[arg(long, default_value_t = 250_000)]
        max_sort_records: usize,
    },
    Drop {
        #[arg(value_name = "COLUMN", required = true, num_args = 2..)]
        columns: Vec<String>,
    },
    Rebuild {
        #[arg(value_name = "COLUMN", required = true, num_args = 2..)]
        columns: Vec<String>,
        #[arg(long, default_value_t = 250_000)]
        max_sort_records: usize,
    },
}

#[derive(Subcommand, Debug)]
enum GenerationCommand {
    List,
    Current,
    Rollback { id: u64 },
    Vacuum {
        #[arg(long, default_value_t = 2)]
        retain: usize,
        #[arg(long = "protect")]
        protected: Vec<u64>,
    },
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

fn logical_predicates(
    predicates: Vec<String>,
    null_columns: Vec<String>,
) -> Result<Vec<LogicalPredicate>, Box<dyn Error>> {
    let mut predicates = predicates
        .iter()
        .map(|x| parse_logical_predicate(x))
        .collect::<Result<Vec<_>, _>>()?;
    predicates.extend(null_columns.into_iter().map(|column| LogicalPredicate {
        column,
        value: None,
    }));
    if predicates.is_empty() {
        return Err("at least one predicate is required".into());
    }
    Ok(predicates)
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
            print_json(&apply_mutations_delta(&cli.root, &mutations, &config)?)?;
        }
        Command::Compact {
            batch_rows,
            max_sort_records,
            dictionary_run_bytes,
        } => {
            let config = CompactionConfig {
                batch_rows,
                max_sort_records,
                dictionary_run_bytes,
            };
            print_json(&compact_dataset(&cli.root, &config)?)?;
        }
        Command::Indexes { command } => match command {
            IndexCommand::List => print_json(&list_indexes(&cli.root)?)?,
            IndexCommand::Add {
                columns,
                max_sort_records,
            } => print_json(&add_index(&cli.root, &columns, max_sort_records)?)?,
            IndexCommand::Drop { columns } => print_json(&drop_index(&cli.root, &columns)?)?,
            IndexCommand::Rebuild {
                columns,
                max_sort_records,
            } => print_json(&rebuild_index(&cli.root, &columns, max_sort_records)?)?,
        },
        Command::Restore { backup } => {
            print_json(&restore_backup(&cli.root, backup)?)?;
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
            GenerationCommand::Vacuum { retain, protected } => {
                print_json(&vacuum_generations(&cli.root, retain, &protected)?)?;
            }
        },
        command => {
            let dataset = resolve_dataset_root(&cli.root)?;
            match command {
                Command::Status => print_json(&dataset_status(&dataset)?)?,
                Command::Stats => print_json(&dataset_stats(&cli.root)?)?,
                Command::Verify => {
                    let report = verify_dataset(&dataset)?;
                    print_json(&report)?;
                    if !report.valid { process::exit(2); }
                }
                Command::Seal => {
                    let seal = seal_dataset(&dataset)?;
                    let report = verify_dataset(&dataset)?;
                    print_json(&json!({
                        "sealed_files": seal.entries.len(),
                        "generation_path": dataset,
                        "verification": report,
                    }))?;
                    if !report.valid { process::exit(2); }
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
                        "stats": stats,
                    }))?;
                }
                Command::Explain { predicates } => {
                    let predicates = predicates
                        .iter()
                        .map(|x| parse_predicate(x))
                        .collect::<Result<Vec<_>, _>>()?;
                    let engine = Engine::open(&dataset)?;
                    print_json(&engine.explain(&predicates))?;
                }
                Command::QueryValues {
                    predicates,
                    null_columns,
                    select,
                    limit,
                } => {
                    let predicates = logical_predicates(predicates, null_columns)?;
                    let logical = VersionedDataset::open(&cli.root)?;
                    let selection = if select.is_empty() { None } else { Some(select.as_slice()) };
                    print_json(&logical.query_values(&predicates, selection, limit)?)?;
                }
                Command::ExplainValues { predicates, null_columns } => {
                    let predicates = logical_predicates(predicates, null_columns)?;
                    let logical = VersionedDataset::open(&cli.root)?;
                    print_json(&logical.explain_values(&predicates)?)?;
                }
                Command::Backup { destination } => {
                    let report = backup_dataset(&dataset, &destination)?;
                    print_json(&json!({
                        "source_generation": dataset,
                        "destination": destination,
                        "verification": report,
                    }))?;
                }
                Command::Import { .. }
                | Command::Mutate { .. }
                | Command::Compact { .. }
                | Command::Indexes { .. }
                | Command::Restore { .. }
                | Command::Generations { .. } => unreachable!(),
            }
        }
    }
    Ok(())
}

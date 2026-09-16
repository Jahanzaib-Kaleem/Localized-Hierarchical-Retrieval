use clap::{Parser, Subcommand};
use lhr::{
    backup_dataset, dataset_status, list_generations, resolve_dataset_root, rollback_generation,
    seal_dataset, verify_dataset, Engine, Predicate,
};
use serde_json::json;
use std::{error::Error, path::PathBuf, process};

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
    /// Run an exact token query. Predicates use COLUMN=VALUE, e.g. 2=17.
    Query {
        #[arg(value_name = "COLUMN=VALUE", required = true)]
        predicates: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
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

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), Box<dyn Error>> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    match cli.command {
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
                        "row_ids": row_ids,
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
                Command::Backup { destination } => {
                    let report = backup_dataset(&dataset, &destination)?;
                    print_json(&json!({
                        "source_generation": dataset,
                        "destination": destination,
                        "verification": report,
                    }))?;
                }
                Command::Generations { .. } => unreachable!(),
            }
        }
    }
    Ok(())
}

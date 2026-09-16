use clap::{Parser, Subcommand, ValueEnum};
use lhr::{
    add_index, apply_mutations_delta, backup_dataset, compact_dataset, dataset_stats,
    dataset_status, drop_index, execute_query, import_csv, import_external, list_generations,
    list_indexes, planner_indexes_for_request, read_schema_file, rebuild_index, record_query,
    recover_catalog, resolve_dataset_root, restore_backup, rollback_generation, seal_dataset,
    serve, vacuum_with_reader_leases, verify_versioned_dataset, workload_report, CompactionConfig,
    CsvImportConfig, DatasetSchema, Engine, ExternalFormat, ExternalImportConfig, LogicalPredicate,
    Mutation, MutationConfig, Predicate, QueryRequest, ServiceConfig, SnapshotLease,
    VersionedDataset,
};
use serde_json::json;
use std::{error::Error, fs, path::PathBuf, process};

#[derive(Parser, Debug)]
#[command(name = "lhr", version, about = "Localized Hierarchical Retrieval operational CLI")]
struct Cli {
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Status,
    Stats,
    Verify,
    Seal,
    Query {
        #[arg(value_name = "COLUMN=VALUE", required = true)]
        predicates: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Explain {
        #[arg(value_name = "COLUMN=VALUE", required = true)]
        predicates: Vec<String>,
    },
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
    ExplainValues {
        #[arg(value_name = "NAME=VALUE")]
        predicates: Vec<String>,
        #[arg(long = "is-null")]
        null_columns: Vec<String>,
    },
    /// Execute the typed exact query protocol from a JSON request file.
    QueryJson {
        file: PathBuf,
        /// Do not append this query to persistent workload telemetry.
        #[arg(long)]
        no_telemetry: bool,
    },
    /// Aggregate persistent query telemetry and recommend useful exact accelerators.
    Workload,
    Import {
        #[command(subcommand)]
        command: ImportCommand,
    },
    Mutate {
        file: PathBuf,
        #[arg(long, default_value_t = 16_384)]
        batch_rows: usize,
        #[arg(long, default_value_t = 250_000)]
        max_sort_records: usize,
        #[arg(long, default_value_t = 67_108_864)]
        dictionary_run_bytes: usize,
    },
    Compact {
        #[arg(long, default_value_t = 16_384)]
        batch_rows: usize,
        #[arg(long, default_value_t = 250_000)]
        max_sort_records: usize,
        #[arg(long, default_value_t = 67_108_864)]
        dictionary_run_bytes: usize,
    },
    Indexes {
        #[command(subcommand)]
        command: IndexCommand,
    },
    Backup { destination: PathBuf },
    Restore { backup: PathBuf },
    /// Find the newest fully verified published generation, repoint CURRENT if necessary,
    /// and remove abandoned staging/work directories.
    Recover,
    Generations {
        #[command(subcommand)]
        command: GenerationCommand,
    },
    /// Run the authenticated HTTP service. Remote cleartext binds are refused unless the config
    /// explicitly states that a trusted TLS reverse proxy/private transport protects the listener.
    Serve {
        /// JSON service configuration. Defaults to secure loopback-only settings.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the configured bind address, e.g. 127.0.0.1:8787.
        #[arg(long)]
        bind: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliExternalFormat { Csv, Jsonl, Json }
impl From<CliExternalFormat> for ExternalFormat {
    fn from(value: CliExternalFormat) -> Self {
        match value {
            CliExternalFormat::Csv => ExternalFormat::Csv,
            CliExternalFormat::Jsonl => ExternalFormat::Jsonl,
            CliExternalFormat::Json => ExternalFormat::Json,
        }
    }
}

#[derive(Subcommand, Debug)]
enum ImportCommand {
    /// Fast strict CSV import.
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
    /// Reject-aware, resumable import for CSV, JSONL, or streaming JSON arrays.
    External {
        source: PathBuf,
        #[arg(long)]
        schema: PathBuf,
        #[arg(long, value_enum)]
        format: CliExternalFormat,
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
        #[arg(long, default_value_t = 0)]
        max_rejects: usize,
        #[arg(long)]
        reject_output: Option<PathBuf>,
        #[arg(long)]
        progress: Option<PathBuf>,
        #[arg(long, default_value_t = 10_000)]
        progress_every: u64,
        #[arg(long)]
        resume_id: Option<String>,
        #[arg(long, default_value_t = 268_435_456)]
        minimum_free_bytes: u64,
        #[arg(long)]
        allow_unknown_json_fields: bool,
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
    Ok(Predicate { column: column.parse()?, value: value.parse()? })
}

fn parse_logical_predicate(raw: &str) -> Result<LogicalPredicate, Box<dyn Error>> {
    let (column, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid predicate {raw:?}; expected NAME=VALUE"))?;
    if column.is_empty() { return Err("logical predicate column cannot be empty".into()); }
    Ok(LogicalPredicate { column: column.to_owned(), value: Some(value.to_owned()) })
}

fn logical_predicates(
    predicates: Vec<String>,
    null_columns: Vec<String>,
) -> Result<Vec<LogicalPredicate>, Box<dyn Error>> {
    let mut predicates = predicates
        .iter()
        .map(|x| parse_logical_predicate(x))
        .collect::<Result<Vec<_>, _>>()?;
    predicates.extend(null_columns.into_iter().map(|column| LogicalPredicate { column, value: None }));
    if predicates.is_empty() { return Err("at least one predicate is required".into()); }
    Ok(predicates)
}

fn parse_indexes(indexes: &[String], schema: &DatasetSchema) -> Result<Vec<Vec<usize>>, Box<dyn Error>> {
    indexes
        .iter()
        .map(|raw| {
            let names: Vec<_> = raw.split(',').map(str::trim).filter(|x| !x.is_empty()).collect();
            if names.len() < 2 { return Err(format!("index {raw:?} must contain at least two column names").into()); }
            names
                .into_iter()
                .map(|name| schema.column_index(name).ok_or_else(|| format!("unknown index column {name:?}").into()))
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
        Command::Serve { config, bind } => {
            let mut config = match config {
                Some(path) => ServiceConfig::from_json_file(path)?,
                None => ServiceConfig::default(),
            };
            if let Some(bind) = bind { config.bind = bind; }
            config.validate()?;
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            runtime.block_on(serve(&cli.root, config))?;
        }
        Command::Import { command } => match command {
            ImportCommand::Csv {
                source, schema, indexes, page_rows, batch_rows, max_sort_records,
                dictionary_run_bytes,
            } => {
                let schema = read_schema_file(schema)?;
                let config = CsvImportConfig {
                    page_rows, batch_rows, max_sort_records, dictionary_run_bytes,
                    accelerators: parse_indexes(&indexes, &schema)?,
                };
                print_json(&import_csv(&cli.root, source, &schema, &config)?)?;
            }
            ImportCommand::External {
                source, schema, format, indexes, page_rows, batch_rows, max_sort_records,
                dictionary_run_bytes, max_rejects, reject_output, progress, progress_every,
                resume_id, minimum_free_bytes, allow_unknown_json_fields,
            } => {
                let schema = read_schema_file(schema)?;
                let engine = CsvImportConfig {
                    page_rows, batch_rows, max_sort_records, dictionary_run_bytes,
                    accelerators: parse_indexes(&indexes, &schema)?,
                };
                let config = ExternalImportConfig {
                    engine,
                    max_rejects,
                    reject_output,
                    progress_path: progress,
                    progress_every,
                    resume_id,
                    minimum_free_bytes,
                    reject_unknown_json_fields: !allow_unknown_json_fields,
                };
                print_json(&import_external(&cli.root, source, format.into(), &schema, &config)?)?;
            }
        },
        Command::QueryJson { file, no_telemetry } => {
            let request: QueryRequest = serde_json::from_slice(&fs::read(file)?)?;
            let lease = SnapshotLease::acquire(&cli.root)?;
            let dataset = VersionedDataset::open(lease.path())?;
            let indexes = planner_indexes_for_request(&dataset, &request)?;
            let response = execute_query(&dataset, &request)?;
            if !no_telemetry { record_query(&cli.root, &request, &response, indexes)?; }
            print_json(&response)?;
        }
        Command::Workload => print_json(&workload_report(&cli.root)?)?,
        Command::Mutate { file, batch_rows, max_sort_records, dictionary_run_bytes } => {
            let mutations: Vec<Mutation> = serde_json::from_slice(&fs::read(file)?)?;
            let config = MutationConfig { batch_rows, max_sort_records, dictionary_run_bytes };
            print_json(&apply_mutations_delta(&cli.root, &mutations, &config)?)?;
        }
        Command::Compact { batch_rows, max_sort_records, dictionary_run_bytes } => {
            let config = CompactionConfig { batch_rows, max_sort_records, dictionary_run_bytes };
            print_json(&compact_dataset(&cli.root, &config)?)?;
        }
        Command::Indexes { command } => match command {
            IndexCommand::List => print_json(&list_indexes(&cli.root)?)?,
            IndexCommand::Add { columns, max_sort_records } => print_json(&add_index(&cli.root, &columns, max_sort_records)?)?,
            IndexCommand::Drop { columns } => print_json(&drop_index(&cli.root, &columns)?)?,
            IndexCommand::Rebuild { columns, max_sort_records } => print_json(&rebuild_index(&cli.root, &columns, max_sort_records)?)?,
        },
        Command::Restore { backup } => print_json(&restore_backup(&cli.root, backup)?)?,
        Command::Recover => print_json(&recover_catalog(&cli.root)?)?,
        Command::Generations { command } => match command {
            GenerationCommand::List => print_json(&list_generations(&cli.root)?)?,
            GenerationCommand::Current => print_json(&json!({ "path": resolve_dataset_root(&cli.root)? }))?,
            GenerationCommand::Rollback { id } => print_json(&rollback_generation(&cli.root, id)?)?,
            GenerationCommand::Vacuum { retain, protected } => {
                print_json(&vacuum_with_reader_leases(&cli.root, retain, &protected)?)?;
            }
        },
        command => {
            let lease = SnapshotLease::acquire(&cli.root)?;
            let dataset = lease.path().to_path_buf();
            match command {
                Command::Status => print_json(&dataset_status(&dataset)?)?,
                Command::Stats => print_json(&dataset_stats(&cli.root)?)?,
                Command::Verify => {
                    let report = verify_versioned_dataset(&dataset)?;
                    print_json(&report)?;
                    if !report.valid { process::exit(2); }
                }
                Command::Seal => {
                    let seal = seal_dataset(&dataset)?;
                    let report = verify_versioned_dataset(&dataset)?;
                    print_json(&json!({ "sealed_files": seal.entries.len(), "generation_path": dataset, "verification": report }))?;
                    if !report.valid { process::exit(2); }
                }
                Command::Query { predicates, limit } => {
                    let predicates = predicates.iter().map(|x| parse_predicate(x)).collect::<Result<Vec<_>, _>>()?;
                    let engine = Engine::open(&dataset)?;
                    let (row_ids, stats) = engine.query_row_ids(&predicates, limit);
                    print_json(&json!({ "generation_path": dataset, "physical_row_ids": row_ids, "returned": row_ids.len(), "limit": limit, "stats": stats }))?;
                }
                Command::Explain { predicates } => {
                    let predicates = predicates.iter().map(|x| parse_predicate(x)).collect::<Result<Vec<_>, _>>()?;
                    print_json(&Engine::open(&dataset)?.explain(&predicates))?;
                }
                Command::QueryValues { predicates, null_columns, select, limit } => {
                    let predicates = logical_predicates(predicates, null_columns)?;
                    let logical = VersionedDataset::open(&dataset)?;
                    let selection = if select.is_empty() { None } else { Some(select.as_slice()) };
                    print_json(&logical.query_values(&predicates, selection, limit)?)?;
                }
                Command::ExplainValues { predicates, null_columns } => {
                    let predicates = logical_predicates(predicates, null_columns)?;
                    print_json(&VersionedDataset::open(&dataset)?.explain_values(&predicates)?)?;
                }
                Command::Backup { destination } => {
                    let report = backup_dataset(&dataset, &destination)?;
                    print_json(&json!({ "source_generation": dataset, "destination": destination, "verification": report }))?;
                }
                Command::Import { .. } | Command::QueryJson { .. } | Command::Workload
                | Command::Mutate { .. } | Command::Compact { .. } | Command::Indexes { .. }
                | Command::Restore { .. } | Command::Recover | Command::Generations { .. }
                | Command::Serve { .. } => unreachable!(),
            }
        }
    }
    Ok(())
}

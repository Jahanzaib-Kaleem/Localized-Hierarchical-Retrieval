pub mod admin;
pub mod bitmap;
pub mod bitslice_postings;
pub mod builder;
pub mod catalog;
pub mod compaction;
pub mod delta_mutation;
pub mod delta_postings;
pub mod dense_postings;
pub mod dictionary;
pub mod engine;
pub mod exact;
pub mod external;
pub mod flat_postings;
pub mod hierarchy;
pub mod import;
pub mod intersect;
pub mod key;
pub mod logical;
pub mod maintenance;
pub mod manifest;
pub mod mutation;
pub mod operations;
pub mod overlay;
pub mod planner;
pub mod postings;
pub mod recovery;
pub mod rowids;
pub mod schema;
pub mod segment;
pub mod snapshot;
pub mod versioned;

pub use admin::{
    add_index, dataset_stats, drop_index, list_indexes, rebuild_index, ColumnStats,
    DatasetStatsReport, IndexChangeReport, IndexInfo,
};
pub use bitmap::BitmapHierarchy;
pub use bitslice_postings::BitSlicePostingHierarchy;
pub use builder::{build_u32_batches, build_u8_batches, BuildConfig, HierarchySpec};
pub use catalog::{
    abandon_generation, begin_generation, list_generations, publish_generation,
    resolve_dataset_root, rollback_generation, vacuum_generations, GenerationInfo,
    StagedGeneration, VacuumReport,
};
pub use compaction::{compact_dataset, CompactionConfig, CompactionReport};
pub use delta_mutation::apply_mutations_delta;
pub use delta_postings::DeltaPostingHierarchy;
pub use dense_postings::DensePostingHierarchy;
pub use dictionary::{write_dictionary_record, DecodedValue, Dictionary};
pub use engine::{Engine, Predicate, QueryExplain, QueryPlanIndex, QueryStats};
pub use exact::add_exact_hierarchies;
pub use flat_postings::FlatPostingHierarchy;
pub use hierarchy::{Hierarchy, Record};
pub use import::{import_csv, CsvImportConfig, CsvImportReport};
pub use intersect::intersect_sorted;
pub use key::mixed_radix_key;
pub use logical::{
    dictionary_filename, LogicalDataset, LogicalExplain, LogicalPredicate, LogicalQueryResult,
    LogicalRow, NamedValue,
};
pub use maintenance::restore_backup;
pub use manifest::{HierarchyMeta, Manifest, SegmentMeta};
pub use mutation::{apply_mutations, Mutation, MutationConfig, MutationReport};
pub use operations::{
    backup_dataset, dataset_status, read_integrity_manifest, seal_dataset, verify_dataset,
    DatasetStatus, IntegrityEntry, IntegrityManifest, VerificationReport,
};
pub use overlay::{
    delta_path, read_overlay, write_overlay, write_visibility, DeltaLayerMeta, OverlayCatalog,
    VisibilityMap, VisibilityTarget, DELTAS_DIR, OVERLAY_FILE, VISIBILITY_FILE,
};
pub use planner::choose_hierarchies;
pub use postings::PostingHierarchy;
pub use recovery::{recover_catalog, verify_versioned_dataset, RecoveryReport};
pub use rowids::{RowIdMap, RowIdWriter, ROW_IDS_FILE};
pub use schema::{
    read_schema, read_schema_file, write_schema, ColumnSchema, DatasetSchema, LogicalType,
    Normalization, SCHEMA_FORMAT,
};
pub use segment::Segment;
pub use snapshot::{
    leased_generation_ids, vacuum_with_reader_leases, SafeVacuumReport, SnapshotLease,
};
pub use versioned::VersionedDataset;

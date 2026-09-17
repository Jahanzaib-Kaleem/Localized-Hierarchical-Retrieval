use crate::{DatasetSchema, LogicalDataset, LogicalPredicate, LogicalQueryResult, LogicalRow};
use std::{io, path::PathBuf};

#[derive(Debug, Clone)]
pub struct ShardSpec {
    pub root: PathBuf,
    /// Global logical row-ID offset for this physical shard. The shard keeps its compact local
    /// physical/index addressing; only the composition layer adds this offset to exposed row IDs.
    pub row_base: u64,
}

struct LoadedShard {
    row_base: u64,
    local_max_row_id: Option<u64>,
    dataset: LogicalDataset,
}

/// Experimental immutable composition layer over ordinary LHR/1 datasets.
///
/// This intentionally does not define a durable shard catalog yet. It proves the query-side
/// invariant first: independent physical datasets can retain their current compact local row
/// addressing while one higher layer exposes a non-overlapping global logical row-ID space.
/// Shards are immutable snapshots for the lifetime of this object.
pub struct ShardedDataset {
    schema: DatasetSchema,
    shards: Vec<LoadedShard>,
    physical_rows: u64,
}

impl ShardedDataset {
    pub fn open(mut specs: Vec<ShardSpec>) -> io::Result<Self> {
        if specs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sharded dataset requires at least one shard",
            ));
        }
        specs.sort_unstable_by_key(|spec| spec.row_base);

        let mut shards = Vec::with_capacity(specs.len());
        let mut schema: Option<DatasetSchema> = None;
        let mut previous_end = None::<u64>;
        let mut physical_rows = 0u64;

        for spec in specs {
            let dataset = LogicalDataset::open(&spec.root)?;
            if let Some(expected) = schema.as_ref() {
                if dataset.schema() != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "physical shard schemas differ",
                    ));
                }
            } else {
                schema = Some(dataset.schema().clone());
            }

            let local_max_row_id = dataset.max_row_id();
            let end = match local_max_row_id {
                Some(local_max) => spec
                    .row_base
                    .checked_add(local_max)
                    .and_then(|value| value.checked_add(1))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "global shard row-ID overflow")
                    })?,
                None => spec.row_base,
            };
            if previous_end.is_some_and(|old_end| spec.row_base < old_end) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "global shard row-ID ranges overlap",
                ));
            }
            previous_end = Some(end);
            physical_rows = physical_rows
                .checked_add(dataset.physical_rows())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "shard row count overflow"))?;
            shards.push(LoadedShard {
                row_base: spec.row_base,
                local_max_row_id,
                dataset,
            });
        }

        Ok(Self {
            schema: schema.unwrap(),
            shards,
            physical_rows,
        })
    }

    pub fn schema(&self) -> &DatasetSchema {
        &self.schema
    }

    pub fn physical_rows(&self) -> u64 {
        self.physical_rows
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn max_row_id(&self) -> Option<u64> {
        self.shards.iter().rev().find_map(|shard| {
            shard
                .local_max_row_id
                .and_then(|local| shard.row_base.checked_add(local))
        })
    }

    /// Query immutable shards in global row-ID order. Total hit count is exact across every shard;
    /// only the requested page is materialized. Shard row-ID ranges are non-overlapping and sorted,
    /// so pages can be concatenated without a cross-shard heap.
    pub fn query_values_after(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        after_row_id: Option<u64>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        let mut hits = 0u64;
        let mut rows_checked = 0u64;
        let mut pages_touched = 0u64;
        let mut hierarchy_lookups = 0u64;
        let mut rows = Vec::<LogicalRow>::with_capacity(limit.min(1024));

        for shard in &self.shards {
            let can_contribute = rows.len() < limit
                && shard.local_max_row_id.is_some_and(|local_max| {
                    let global_max = shard.row_base.saturating_add(local_max);
                    after_row_id.map_or(true, |cursor| cursor < global_max)
                });

            let local_after = if can_contribute {
                after_row_id.and_then(|cursor| {
                    if cursor < shard.row_base {
                        None
                    } else {
                        Some(cursor - shard.row_base)
                    }
                })
            } else {
                shard.local_max_row_id
            };
            let remaining = if can_contribute {
                limit.saturating_sub(rows.len())
            } else {
                0
            };
            let result = shard
                .dataset
                .query_values_after(predicates, select, local_after, remaining)?;
            hits = hits.saturating_add(result.hits);
            rows_checked = rows_checked.saturating_add(result.rows_checked);
            pages_touched = pages_touched.saturating_add(result.pages_touched);
            hierarchy_lookups = hierarchy_lookups.saturating_add(result.hierarchy_lookups);

            if can_contribute {
                for mut row in result.rows {
                    row.row_id = shard.row_base.checked_add(row.row_id).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "global shard row-ID overflow")
                    })?;
                    rows.push(row);
                }
            }
        }

        Ok(LogicalQueryResult {
            hits,
            returned: rows.len(),
            rows_checked,
            pages_touched,
            hierarchy_lookups,
            rows,
        })
    }

    pub fn query_values(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        self.query_values_after(predicates, select, None, limit)
    }
}

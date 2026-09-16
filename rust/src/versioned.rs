use crate::{
    read_overlay, DeltaLayerMeta, LogicalDataset, LogicalExplain, LogicalPredicate, LogicalQueryResult,
    LogicalRow, OverlayCatalog, SnapshotLease, VisibilityMap, VisibilityTarget,
};
use std::{
    io,
    path::{Path, PathBuf},
};

struct Layer {
    id: u32,
    dataset: LogicalDataset,
}

/// A logical view over one immutable base generation plus zero or more immutable delta layers.
/// Visibility overrides select the newest row version (or a tombstone) by stable logical row ID.
/// A shared generation lease pins the resolved CURRENT snapshot for the lifetime of this object.
pub struct VersionedDataset {
    root: PathBuf,
    base: LogicalDataset,
    deltas: Vec<Layer>,
    overlay: OverlayCatalog,
    visibility: VisibilityMap,
    _snapshot: SnapshotLease,
}

impl VersionedDataset {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let snapshot = SnapshotLease::acquire(root)?;
        let root = snapshot.path().to_path_buf();
        let base = LogicalDataset::open(&root)?;
        let overlay = read_overlay(&root, base.physical_rows(), base.max_row_id())?;
        let visibility = VisibilityMap::open_optional(&root)?;
        let mut deltas = Vec::with_capacity(overlay.deltas.len());
        for meta in &overlay.deltas {
            let dataset = LogicalDataset::open(root.join(&meta.path))?;
            if dataset.schema() != base.schema() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("delta layer {} schema differs from base schema", meta.id),
                ));
            }
            if dataset.physical_rows() != meta.rows {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("delta layer {} row count mismatch", meta.id),
                ));
            }
            deltas.push(Layer { id: meta.id, dataset });
        }
        for index in 0..visibility.len() {
            let Some((_, target)) = visibility.entry(index) else { continue; };
            if let VisibilityTarget::Layer(layer) = target {
                if !deltas.iter().any(|x| x.id == layer) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("visibility map references missing delta layer {layer}"),
                    ));
                }
            }
        }
        Ok(Self {
            root,
            base,
            deltas,
            overlay,
            visibility,
            _snapshot: snapshot,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn schema(&self) -> &crate::DatasetSchema {
        self.base.schema()
    }
    pub fn visible_rows(&self) -> u64 {
        self.overlay.visible_rows
    }
    pub fn max_row_id(&self) -> Option<u64> {
        self.overlay.max_row_id
    }
    pub fn overlay(&self) -> &OverlayCatalog {
        &self.overlay
    }
    pub fn visibility(&self) -> &VisibilityMap {
        &self.visibility
    }

    pub fn contains_canonical_value(&self, column: usize, value: &str) -> bool {
        self.base.contains_canonical_value(column, value)
            || self
                .deltas
                .iter()
                .any(|x| x.dataset.contains_canonical_value(column, value))
    }

    fn layer(&self, id: u32) -> Option<&LogicalDataset> {
        if id == 0 {
            return Some(&self.base);
        }
        self.deltas
            .iter()
            .find(|x| x.id == id)
            .map(|x| &x.dataset)
    }

    fn visible_in_layer(&self, row_id: u64, layer: u32) -> bool {
        match self.visibility.target(row_id) {
            Some(VisibilityTarget::Deleted) => false,
            Some(VisibilityTarget::Layer(target)) => target == layer,
            None => true,
        }
    }

    pub fn row_values(&self, row_id: u64) -> io::Result<Option<Vec<Option<String>>>> {
        match self.visibility.target(row_id) {
            Some(VisibilityTarget::Deleted) => Ok(None),
            Some(VisibilityTarget::Layer(layer)) => {
                let dataset = self.layer(layer).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "visibility target layer is missing")
                })?;
                let Some(physical) = dataset.physical_row_id(row_id) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("visibility layer {layer} does not contain row {row_id}"),
                    ));
                };
                Ok(Some(dataset.decode_physical_values(physical)?))
            }
            None => {
                for layer in self.deltas.iter().rev() {
                    if let Some(physical) = layer.dataset.physical_row_id(row_id) {
                        return Ok(Some(layer.dataset.decode_physical_values(physical)?));
                    }
                }
                if let Some(physical) = self.base.physical_row_id(row_id) {
                    return Ok(Some(self.base.decode_physical_values(physical)?));
                }
                Ok(None)
            }
        }
    }

    fn values_match(
        &self,
        values: &[Option<String>],
        predicates: &[LogicalPredicate],
    ) -> io::Result<bool> {
        for predicate in predicates {
            let column = self
                .schema()
                .column_index(&predicate.column)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown column {}", predicate.column),
                    )
                })?;
            let schema = &self.schema().columns[column];
            let expected = match predicate.value.as_deref() {
                None => {
                    if !schema.nullable {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("column {} is not nullable", schema.name),
                        ));
                    }
                    None
                }
                Some(raw) if schema.is_null_literal(raw) => None,
                Some(raw) => Some(schema.canonicalize(raw)?),
            };
            if values.get(column) != Some(&expected) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn hidden_match_count(
        &self,
        layer_id: u32,
        dataset: &LogicalDataset,
        predicates: &[LogicalPredicate],
    ) -> io::Result<(u64, usize)> {
        let mut hidden_hits = 0u64;
        let mut hidden_rows = 0usize;
        for index in 0..self.visibility.len() {
            let Some((row_id, target)) = self.visibility.entry(index) else { continue; };
            if matches!(target, VisibilityTarget::Layer(target_layer) if target_layer == layer_id) {
                continue;
            }
            let Some(physical) = dataset.physical_row_id(row_id) else { continue; };
            hidden_rows = hidden_rows.saturating_add(1);
            let values = dataset.decode_physical_values(physical)?;
            if self.values_match(&values, predicates)? {
                hidden_hits += 1;
            }
        }
        Ok((hidden_hits, hidden_rows))
    }

    pub fn explain_values(
        &self,
        predicates: &[LogicalPredicate],
    ) -> io::Result<Vec<(u32, LogicalExplain)>> {
        let mut plans = Vec::with_capacity(self.deltas.len() + 1);
        plans.push((0, self.base.explain_values(predicates)?));
        for layer in &self.deltas {
            plans.push((layer.id, layer.dataset.explain_values(predicates)?));
        }
        Ok(plans)
    }

    pub fn query_values(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        let mut hits = 0u64;
        let mut rows_checked = 0u64;
        let mut pages_touched = 0u64;
        let mut hierarchy_lookups = 0u64;
        let mut rows = Vec::<LogicalRow>::new();

        let mut run_layer = |layer_id: u32, dataset: &LogicalDataset| -> io::Result<()> {
            let (hidden_hits, hidden_rows) =
                self.hidden_match_count(layer_id, dataset, predicates)?;
            let fetch_limit = limit.saturating_add(hidden_rows);
            let result = dataset.query_values(predicates, select, fetch_limit)?;
            hits = hits.saturating_add(result.hits.saturating_sub(hidden_hits));
            rows_checked = rows_checked.saturating_add(result.rows_checked);
            pages_touched = pages_touched.saturating_add(result.pages_touched);
            hierarchy_lookups = hierarchy_lookups.saturating_add(result.hierarchy_lookups);
            rows.extend(
                result
                    .rows
                    .into_iter()
                    .filter(|row| self.visible_in_layer(row.row_id, layer_id)),
            );
            Ok(())
        };

        run_layer(0, &self.base)?;
        for layer in &self.deltas {
            run_layer(layer.id, &layer.dataset)?;
        }
        rows.sort_unstable_by_key(|row| row.row_id);
        rows.dedup_by_key(|row| row.row_id);
        if rows.len() > limit {
            rows.truncate(limit);
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

    /// Stream visible rows in stable logical-row-ID order without collecting the whole database.
    pub fn for_each_visible_row<F>(&self, mut f: F) -> io::Result<()>
    where
        F: FnMut(u64, Vec<Option<String>>) -> io::Result<()>,
    {
        let mut positions = vec![0u64; self.deltas.len() + 1];
        loop {
            let mut next: Option<u64> = None;
            for (index, position) in positions.iter().enumerate() {
                let dataset = if index == 0 {
                    &self.base
                } else {
                    &self.deltas[index - 1].dataset
                };
                if *position >= dataset.physical_rows() {
                    continue;
                }
                let id = dataset.logical_row_id(*position).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "layer row is missing logical row ID",
                    )
                })?;
                next = Some(next.map_or(id, |old| old.min(id)));
            }
            let Some(row_id) = next else { break; };

            for (index, position) in positions.iter_mut().enumerate() {
                let dataset = if index == 0 {
                    &self.base
                } else {
                    &self.deltas[index - 1].dataset
                };
                while *position < dataset.physical_rows()
                    && dataset.logical_row_id(*position) == Some(row_id)
                {
                    *position += 1;
                }
            }
            if let Some(values) = self.row_values(row_id)? {
                f(row_id, values)?;
            }
        }
        Ok(())
    }

    pub fn delta_meta(&self) -> &[DeltaLayerMeta] {
        &self.overlay.deltas
    }
}

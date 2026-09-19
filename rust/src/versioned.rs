use crate::{
    read_overlay, DeltaLayerMeta, LogicalDataset, LogicalExplain, LogicalPredicate, LogicalQueryResult,
    LogicalRow, NamedValue, OverlayCatalog, SnapshotLease, VisibilityMap, VisibilityTarget,
};
use std::{
    io,
    path::{Path, PathBuf},
};

struct Layer {
    id: u32,
    dataset: LogicalDataset,
    logical_to_physical: Vec<Option<usize>>,
}

fn merge_layer_schemas(
    base: &crate::DatasetSchema,
    delta_schemas: &[crate::DatasetSchema],
) -> io::Result<crate::DatasetSchema> {
    let mut columns = base.columns.clone();
    for incoming in delta_schemas {
        for column in &incoming.columns {
            if let Some(index) = columns.iter().position(|existing| existing.name == column.name) {
                let existing = &mut columns[index];
                if existing.logical_type != column.logical_type
                    || existing.normalization != column.normalization
                    || existing.null_values != column.null_values
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "schema evolution conflict for column {:?}: type, normalization, and null literals must remain stable",
                            column.name
                        ),
                    ));
                }
                existing.nullable |= column.nullable;
            } else {
                let mut evolved = column.clone();
                // Every earlier layer lacks this newly introduced column.
                evolved.nullable = true;
                columns.push(evolved);
            }
        }
    }

    let mut schemas = Vec::with_capacity(delta_schemas.len() + 1);
    schemas.push(base);
    schemas.extend(delta_schemas.iter());
    for column in &mut columns {
        if schemas
            .iter()
            .any(|schema| schema.column_index(&column.name).is_none())
        {
            column.nullable = true;
        }
    }
    crate::DatasetSchema::new(columns)
}

fn schema_map(
    logical: &crate::DatasetSchema,
    physical: &crate::DatasetSchema,
) -> Vec<Option<usize>> {
    logical
        .columns
        .iter()
        .map(|column| physical.column_index(&column.name))
        .collect()
}

/// A logical view over one immutable base generation plus zero or more immutable delta layers.
/// Delta layers may add or omit named columns. The logical schema is their deterministic union:
/// shared columns keep stable type/normalization semantics, while columns absent from any layer are
/// nullable and read as NULL for that layer. Existing immutable rows never need rewriting merely
/// because a later append introduces a new column.
pub struct VersionedDataset {
    root: PathBuf,
    base: LogicalDataset,
    base_logical_to_physical: Vec<Option<usize>>,
    deltas: Vec<Layer>,
    schema: crate::DatasetSchema,
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

        let mut physical_deltas = Vec::with_capacity(overlay.deltas.len());
        for meta in &overlay.deltas {
            let dataset = LogicalDataset::open(root.join(&meta.path))?;
            if dataset.physical_rows() != meta.rows {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("delta layer {} row count mismatch", meta.id),
                ));
            }
            physical_deltas.push((meta.id, dataset));
        }

        let delta_schemas = physical_deltas
            .iter()
            .map(|(_, dataset)| dataset.schema().clone())
            .collect::<Vec<_>>();
        let schema = merge_layer_schemas(base.schema(), &delta_schemas)?;
        let base_logical_to_physical = schema_map(&schema, base.schema());
        let deltas = physical_deltas
            .into_iter()
            .map(|(id, dataset)| {
                let logical_to_physical = schema_map(&schema, dataset.schema());
                Layer {
                    id,
                    dataset,
                    logical_to_physical,
                }
            })
            .collect::<Vec<_>>();

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
            base_logical_to_physical,
            deltas,
            schema,
            overlay,
            visibility,
            _snapshot: snapshot,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn schema(&self) -> &crate::DatasetSchema {
        &self.schema
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
        if column >= self.schema.columns.len() {
            return false;
        }
        if let Some(physical) = self.base_logical_to_physical[column] {
            if self.base.contains_canonical_value(physical, value) {
                return true;
            }
        }
        self.deltas.iter().any(|layer| {
            layer.logical_to_physical[column]
                .map(|physical| layer.dataset.contains_canonical_value(physical, value))
                .unwrap_or(false)
        })
    }

    pub fn has_exact_singleton(&self, column: usize) -> bool {
        if column >= self.schema.columns.len() {
            return false;
        }
        let mut present = false;
        if let Some(physical) = self.base_logical_to_physical[column] {
            present = true;
            if !self.base.has_exact_singleton(physical) {
                return false;
            }
        }
        for layer in &self.deltas {
            if let Some(physical) = layer.logical_to_physical[column] {
                present = true;
                if !layer.dataset.has_exact_singleton(physical) {
                    return false;
                }
            }
        }
        present
    }

    fn layer_with_map(&self, id: u32) -> Option<(&LogicalDataset, &[Option<usize>])> {
        if id == 0 {
            return Some((&self.base, &self.base_logical_to_physical));
        }
        self.deltas
            .iter()
            .find(|x| x.id == id)
            .map(|x| (&x.dataset, x.logical_to_physical.as_slice()))
    }

    fn visible_in_layer(&self, row_id: u64, layer: u32) -> bool {
        match self.visibility.target(row_id) {
            Some(VisibilityTarget::Deleted) => false,
            Some(VisibilityTarget::Layer(target)) => target == layer,
            None => true,
        }
    }

    fn align_values(
        &self,
        map: &[Option<usize>],
        physical_values: &[Option<String>],
    ) -> Vec<Option<String>> {
        map.iter()
            .map(|physical| physical.and_then(|index| physical_values.get(index).cloned().flatten()))
            .collect()
    }

    fn projection_indices(&self, select: Option<&[String]>) -> io::Result<Vec<usize>> {
        match select {
            None => Ok((0..self.schema.columns.len()).collect()),
            Some(names) => names
                .iter()
                .map(|name| {
                    self.schema.column_index(name).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown selected column {name}"),
                        )
                    })
                })
                .collect(),
        }
    }

    fn predicates_for_layer(
        &self,
        dataset: &LogicalDataset,
        map: &[Option<usize>],
        predicates: &[LogicalPredicate],
    ) -> io::Result<Option<Vec<LogicalPredicate>>> {
        let mut out = Vec::with_capacity(predicates.len());
        for predicate in predicates {
            let logical = self.schema.column_index(&predicate.column).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown column {}", predicate.column),
                )
            })?;
            let logical_column = &self.schema.columns[logical];
            let is_null = match predicate.value.as_deref() {
                None => {
                    if !logical_column.nullable {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("column {} is not nullable", logical_column.name),
                        ));
                    }
                    true
                }
                Some(raw) => logical_column.is_null_literal(raw),
            };

            let Some(physical) = map[logical] else {
                if is_null {
                    continue;
                }
                return Ok(None);
            };
            let physical_column = &dataset.schema().columns[physical];
            if is_null && !physical_column.nullable {
                return Ok(None);
            }
            out.push(predicate.clone());
        }
        Ok(Some(out))
    }

    fn logical_row(
        &self,
        row: LogicalRow,
        _map: &[Option<usize>],
        projection: &[usize],
    ) -> LogicalRow {
        let mut values = Vec::with_capacity(projection.len());
        for &logical in projection {
            let name = self.schema.columns[logical].name.as_str();
            let value = row
                .values
                .iter()
                .find(|value| value.column.as_str() == name)
                .and_then(|value| value.value.clone());
            values.push(NamedValue {
                column: self.schema.columns[logical].name.clone(),
                value,
            });
        }
        LogicalRow {
            row_id: row.row_id,
            values,
        }
    }

    pub fn row_values(&self, row_id: u64) -> io::Result<Option<Vec<Option<String>>>> {
        match self.visibility.target(row_id) {
            Some(VisibilityTarget::Deleted) => Ok(None),
            Some(VisibilityTarget::Layer(layer)) => {
                let (dataset, map) = self.layer_with_map(layer).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "visibility target layer is missing")
                })?;
                let Some(physical) = dataset.physical_row_id(row_id) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("visibility layer {layer} does not contain row {row_id}"),
                    ));
                };
                let values = dataset.decode_physical_values(physical)?;
                Ok(Some(self.align_values(map, &values)))
            }
            None => {
                for layer in self.deltas.iter().rev() {
                    if let Some(physical) = layer.dataset.physical_row_id(row_id) {
                        let values = layer.dataset.decode_physical_values(physical)?;
                        return Ok(Some(self.align_values(&layer.logical_to_physical, &values)));
                    }
                }
                if let Some(physical) = self.base.physical_row_id(row_id) {
                    let values = self.base.decode_physical_values(physical)?;
                    return Ok(Some(self.align_values(&self.base_logical_to_physical, &values)));
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
            let column = self.schema().column_index(&predicate.column).ok_or_else(|| {
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
        map: &[Option<usize>],
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
            let physical_values = dataset.decode_physical_values(physical)?;
            let values = self.align_values(map, &physical_values);
            if self.values_match(&values, predicates)? {
                hidden_hits += 1;
            }
        }
        Ok((hidden_hits, hidden_rows))
    }

    fn hidden_row_count(
        &self,
        layer_id: u32,
        dataset: &LogicalDataset,
    ) -> usize {
        let mut hidden_rows = 0usize;
        for index in 0..self.visibility.len() {
            let Some((row_id, target)) = self.visibility.entry(index) else { continue; };
            if matches!(target, VisibilityTarget::Layer(target_layer) if target_layer == layer_id) {
                continue;
            }
            if dataset.physical_row_id(row_id).is_some() {
                hidden_rows = hidden_rows.saturating_add(1);
            }
        }
        hidden_rows
    }

    pub fn explain_values(
        &self,
        predicates: &[LogicalPredicate],
    ) -> io::Result<Vec<(u32, LogicalExplain)>> {
        let mut plans = Vec::with_capacity(self.deltas.len() + 1);
        let mut explain_layer =
            |id: u32, dataset: &LogicalDataset, map: &[Option<usize>]| -> io::Result<()> {
                let explain = match self.predicates_for_layer(dataset, map, predicates)? {
                    None => LogicalExplain {
                        predicates: predicates.to_vec(),
                        dictionary_miss: true,
                        plan: None,
                    },
                    Some(layer_predicates) => {
                        let mut explain = dataset.explain_values(&layer_predicates)?;
                        explain.predicates = predicates.to_vec();
                        explain
                    }
                };
                plans.push((id, explain));
                Ok(())
            };
        explain_layer(0, &self.base, &self.base_logical_to_physical)?;
        for layer in &self.deltas {
            explain_layer(layer.id, &layer.dataset, &layer.logical_to_physical)?;
        }
        Ok(plans)
    }

    pub fn query_values(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        self.query_values_after(predicates, select, None, limit)
    }

    pub fn query_values_after(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        after_row_id: Option<u64>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        let projection = self.projection_indices(select)?;
        let mut hits = 0u64;
        let mut rows_checked = 0u64;
        let mut pages_touched = 0u64;
        let mut hierarchy_lookups = 0u64;
        let mut rows = Vec::<LogicalRow>::new();

        let mut run_layer =
            |layer_id: u32, dataset: &LogicalDataset, map: &[Option<usize>]| -> io::Result<()> {
                let Some(layer_predicates) =
                    self.predicates_for_layer(dataset, map, predicates)?
                else {
                    return Ok(());
                };
                let (hidden_hits, hidden_rows) =
                    self.hidden_match_count(layer_id, dataset, map, predicates)?;
                let fetch_limit = limit.saturating_add(hidden_rows);
                // Fetch the complete physical row so it can be projected into the union schema.
                let result =
                    dataset.query_values_after(&layer_predicates, None, after_row_id, fetch_limit)?;
                hits = hits.saturating_add(result.hits.saturating_sub(hidden_hits));
                rows_checked = rows_checked.saturating_add(result.rows_checked);
                pages_touched = pages_touched.saturating_add(result.pages_touched);
                hierarchy_lookups = hierarchy_lookups.saturating_add(result.hierarchy_lookups);
                rows.extend(
                    result
                        .rows
                        .into_iter()
                        .filter(|row| self.visible_in_layer(row.row_id, layer_id))
                        .map(|row| self.logical_row(row, map, &projection)),
                );
                Ok(())
            };

        run_layer(0, &self.base, &self.base_logical_to_physical)?;
        for layer in &self.deltas {
            run_layer(layer.id, &layer.dataset, &layer.logical_to_physical)?;
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

    /// Internal page-only equality route used by residual-filter execution. It preserves
    /// versioned visibility and logical row ordering, but deliberately avoids the global hit-count
    /// work performed by query_values_after.
    pub(crate) fn query_values_page_after(
        &self,
        predicates: &[LogicalPredicate],
        select: Option<&[String]>,
        after_row_id: Option<u64>,
        limit: usize,
    ) -> io::Result<LogicalQueryResult> {
        let projection = self.projection_indices(select)?;
        let mut rows_checked = 0u64;
        let mut pages_touched = 0u64;
        let mut hierarchy_lookups = 0u64;
        let mut rows = Vec::<LogicalRow>::new();

        let mut run_layer =
            |layer_id: u32, dataset: &LogicalDataset, map: &[Option<usize>]| -> io::Result<()> {
                let Some(layer_predicates) =
                    self.predicates_for_layer(dataset, map, predicates)?
                else {
                    return Ok(());
                };
                let hidden_rows = self.hidden_row_count(layer_id, dataset);
                let fetch_limit = limit.saturating_add(hidden_rows);
                let layer_select = projection
                    .iter()
                    .filter_map(|&logical| {
                        map[logical].map(|physical| dataset.schema().columns[physical].name.clone())
                    })
                    .collect::<Vec<_>>();
                let result = dataset.query_values_page_after(
                    &layer_predicates,
                    Some(&layer_select),
                    after_row_id,
                    fetch_limit,
                )?;
                rows_checked = rows_checked.saturating_add(result.rows_checked);
                pages_touched = pages_touched.saturating_add(result.pages_touched);
                hierarchy_lookups = hierarchy_lookups.saturating_add(result.hierarchy_lookups);
                rows.extend(
                    result
                        .rows
                        .into_iter()
                        .filter(|row| self.visible_in_layer(row.row_id, layer_id))
                        .map(|row| self.logical_row(row, map, &projection)),
                );
                Ok(())
            };

        run_layer(0, &self.base, &self.base_logical_to_physical)?;
        for layer in &self.deltas {
            run_layer(layer.id, &layer.dataset, &layer.logical_to_physical)?;
        }
        rows.sort_unstable_by_key(|row| row.row_id);
        rows.dedup_by_key(|row| row.row_id);
        if rows.len() > limit {
            rows.truncate(limit);
        }
        let returned = rows.len();

        Ok(LogicalQueryResult {
            hits: returned as u64,
            returned,
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

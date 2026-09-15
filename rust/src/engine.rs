use crate::{
    mixed_radix_key, BitSlicePostingHierarchy, BitmapHierarchy, DeltaPostingHierarchy,
    DensePostingHierarchy, FlatPostingHierarchy, Hierarchy, Manifest, PostingHierarchy, Segment,
};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Predicate {
    pub column: usize,
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub hits: u64,
    pub rows_checked: u64,
    pub pages_touched: u64,
    pub hierarchy_lookups: u64,
}

enum PageHierarchyData {
    Sparse(Hierarchy),
    Bitmap(BitmapHierarchy),
}
impl PageHierarchyData {
    fn page_count(&self, key: u64) -> usize {
        match self {
            Self::Sparse(x) => x.page_count(key),
            Self::Bitmap(x) => x.page_count(key),
        }
    }
    fn pages(&self, key: u64) -> Vec<u32> {
        match self {
            Self::Sparse(x) => x.pages(key),
            Self::Bitmap(x) => x.pages(key),
        }
    }
    fn intersect_pages(&self, key: u64, seed: &[u32]) -> Vec<u32> {
        match self {
            Self::Sparse(x) => x.intersect_pages(key, seed),
            Self::Bitmap(x) => x.intersect_pages(key, seed),
        }
    }
}

enum RowHierarchyData {
    Sparse(PostingHierarchy),
    Dense(DensePostingHierarchy),
    Delta(DeltaPostingHierarchy),
    Flat(FlatPostingHierarchy),
    BitSlice(BitSlicePostingHierarchy),
}
impl RowHierarchyData {
    fn row_count(&self, key: u64) -> usize {
        match self {
            Self::Sparse(x) => x.row_count(key),
            Self::Dense(x) => x.row_count(key),
            Self::Delta(x) => x.row_count(key),
            Self::Flat(x) => x.row_count(key),
            Self::BitSlice(x) => x.row_count(key),
        }
    }
    fn rows(&self, key: u64) -> Vec<u32> {
        match self {
            Self::Sparse(x) => x.rows(key),
            Self::Dense(x) => x.rows(key),
            Self::Delta(x) => x.rows(key),
            Self::Flat(x) => x.rows(key),
            Self::BitSlice(x) => x.rows(key),
        }
    }
    fn intersect_rows(&self, key: u64, seed: &[u32]) -> Vec<u32> {
        match self {
            Self::Sparse(x) => x.intersect_rows(key, seed),
            Self::Dense(x) => x.intersect_rows(key, seed),
            Self::Delta(x) => x.intersect_rows(key, seed),
            Self::Flat(x) => x.intersect_rows(key, seed),
            Self::BitSlice(x) => x.intersect_rows(key, seed),
        }
    }
    fn bitslice(&self) -> Option<&BitSlicePostingHierarchy> {
        match self {
            Self::BitSlice(x) => Some(x),
            _ => None,
        }
    }
}

struct LoadedPageHierarchy {
    columns: Vec<usize>,
    data: PageHierarchyData,
}
struct LoadedRowHierarchy {
    columns: Vec<usize>,
    data: RowHierarchyData,
}
struct LoadedSegment {
    first_page: u32,
    row_start: u64,
    data: Segment,
}
struct RowPlan {
    rows: Vec<u32>,
    lookups: u64,
    fully_covered: bool,
}

pub struct Engine {
    rows: u64,
    page_rows: usize,
    pages: u32,
    columns: usize,
    card: Vec<u64>,
    page_hier: Vec<LoadedPageHierarchy>,
    row_hier: Vec<LoadedRowHierarchy>,
    segments: Vec<LoadedSegment>,
}

impl Engine {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let raw = fs::read(root.join("manifest.json"))?;
        let manifest: Manifest = serde_json::from_slice(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if !manifest.format.starts_with("LHR/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported LHR manifest",
            ));
        }

        let mut page_hier = Vec::new();
        let mut row_hier = Vec::new();
        for hierarchy in &manifest.hierarchies {
            let path = root.join("routing").join(&hierarchy.file);
            match hierarchy.kind.as_str() {
                "sparse" => page_hier.push(LoadedPageHierarchy {
                    columns: hierarchy.columns.clone(),
                    data: PageHierarchyData::Sparse(Hierarchy::open(path)?),
                }),
                "bitmap" => page_hier.push(LoadedPageHierarchy {
                    columns: hierarchy.columns.clone(),
                    data: PageHierarchyData::Bitmap(BitmapHierarchy::open(path)?),
                }),
                "postings" => row_hier.push(LoadedRowHierarchy {
                    columns: hierarchy.columns.clone(),
                    data: RowHierarchyData::Sparse(PostingHierarchy::open(path)?),
                }),
                "densepost" => row_hier.push(LoadedRowHierarchy {
                    columns: hierarchy.columns.clone(),
                    data: RowHierarchyData::Dense(DensePostingHierarchy::open(path)?),
                }),
                "deltapost" => row_hier.push(LoadedRowHierarchy {
                    columns: hierarchy.columns.clone(),
                    data: RowHierarchyData::Delta(DeltaPostingHierarchy::open(path)?),
                }),
                "flatpost" => row_hier.push(LoadedRowHierarchy {
                    columns: hierarchy.columns.clone(),
                    data: RowHierarchyData::Flat(FlatPostingHierarchy::open(path)?),
                }),
                "bitslice" => row_hier.push(LoadedRowHierarchy {
                    columns: hierarchy.columns.clone(),
                    data: RowHierarchyData::BitSlice(BitSlicePostingHierarchy::open(path)?),
                }),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown hierarchy kind {other}"),
                    ))
                }
            }
        }

        let mut segments = Vec::new();
        for segment in &manifest.segments {
            let data = Segment::open(root.join("canonical").join(&segment.file))?;
            if data.rows() != segment.rows as usize || data.cols() != manifest.columns {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "segment metadata mismatch",
                ));
            }
            segments.push(LoadedSegment {
                first_page: segment.first_page,
                row_start: segment.row_start,
                data,
            });
        }

        Ok(Self {
            rows: manifest.rows,
            page_rows: manifest.page_rows,
            pages: manifest.pages,
            columns: manifest.columns,
            card: manifest.cardinalities,
            page_hier,
            row_hier,
            segments,
        })
    }

    fn query_map(&self, predicates: &[Predicate]) -> Option<HashMap<usize, u64>> {
        let mut query = HashMap::new();
        for predicate in predicates {
            if predicate.column >= self.columns
                || self
                    .card
                    .get(predicate.column)
                    .map_or(true, |&k| predicate.value >= k)
            {
                return None;
            }
            if let Some(old) = query.insert(predicate.column, predicate.value) {
                if old != predicate.value {
                    return None;
                }
            }
        }
        Some(query)
    }

    fn hierarchy_key(&self, columns: &[usize], query: &HashMap<usize, u64>) -> Option<u64> {
        let mut values = Vec::with_capacity(columns.len());
        for &column in columns {
            values.push((column, *query.get(&column)?));
        }
        mixed_radix_key(&values, &self.card)
    }

    fn candidate_rows_with_lookups(&self, predicates: &[Predicate]) -> Option<RowPlan> {
        let query = match self.query_map(predicates) {
            Some(query) => query,
            None => {
                return Some(RowPlan {
                    rows: vec![],
                    lookups: 0,
                    fully_covered: true,
                })
            }
        };

        #[derive(Clone)]
        struct Candidate {
            count: usize,
            index: usize,
            key: u64,
            columns: Vec<usize>,
        }

        let mut available = Vec::new();
        for (index, hierarchy) in self.row_hier.iter().enumerate() {
            if hierarchy.columns.iter().all(|c| query.contains_key(c)) {
                let key = self.hierarchy_key(&hierarchy.columns, &query)?;
                let count = hierarchy.data.row_count(key);
                if count == 0 {
                    return Some(RowPlan {
                        rows: vec![],
                        lookups: 1,
                        fully_covered: true,
                    });
                }
                available.push(Candidate {
                    count,
                    index,
                    key,
                    columns: hierarchy.columns.clone(),
                });
            }
        }
        if available.is_empty() {
            return None;
        }

        let considered = available.len() as u64;
        let mut uncovered: HashSet<usize> = query.keys().copied().collect();
        let mut selected = Vec::new();
        let mut used = vec![false; available.len()];
        while !uncovered.is_empty() {
            let mut best = None;
            for (candidate_index, candidate) in available.iter().enumerate() {
                if used[candidate_index] {
                    continue;
                }
                let gain = candidate
                    .columns
                    .iter()
                    .filter(|column| uncovered.contains(column))
                    .count();
                if gain == 0 {
                    continue;
                }
                match best {
                    None => best = Some((candidate_index, gain, candidate.count)),
                    Some((_, best_gain, best_count)) => {
                        if (candidate.count as u128) * (best_gain as u128)
                            < (best_count as u128) * (gain as u128)
                        {
                            best = Some((candidate_index, gain, candidate.count));
                        }
                    }
                }
            }
            let Some((candidate_index, _, _)) = best else {
                break;
            };
            used[candidate_index] = true;
            for column in &available[candidate_index].columns {
                uncovered.remove(column);
            }
            selected.push(available[candidate_index].clone());
        }

        selected.sort_unstable_by_key(|x| x.count);
        let first = selected.first()?;

        // Low-cardinality singleton indexes are bit-sliced. When composition starts with two
        // such indexes, intersect their equality masks word-at-a-time instead of materializing
        // either huge singleton posting list.
        let (mut rows, next) = if selected.len() >= 2 {
            let second = &selected[1];
            let first_data = &self.row_hier[first.index].data;
            let second_data = &self.row_hier[second.index].data;
            if let (Some(a), Some(b)) = (first_data.bitslice(), second_data.bitslice()) {
                (a.intersect_hierarchy(first.key, b, second.key), 2usize)
            } else {
                (first_data.rows(first.key), 1usize)
            }
        } else {
            (self.row_hier[first.index].data.rows(first.key), 1usize)
        };

        for candidate in &selected[next..] {
            rows = self.row_hier[candidate.index]
                .data
                .intersect_rows(candidate.key, &rows);
            if rows.is_empty() {
                break;
            }
        }

        Some(RowPlan {
            rows,
            lookups: considered,
            fully_covered: uncovered.is_empty(),
        })
    }

    fn candidate_pages_with_lookups(&self, predicates: &[Predicate]) -> (Vec<u32>, u64) {
        let query = match self.query_map(predicates) {
            Some(query) => query,
            None => return (vec![], 0),
        };
        let mut available = Vec::new();
        for (index, hierarchy) in self.page_hier.iter().enumerate() {
            if hierarchy.columns.iter().all(|c| query.contains_key(c)) {
                let Some(key) = self.hierarchy_key(&hierarchy.columns, &query) else {
                    return (vec![], 0);
                };
                let count = hierarchy.data.page_count(key);
                if count == 0 {
                    return (vec![], 1);
                }
                available.push((count, index, key));
            }
        }
        let lookups = available.len() as u64;
        if available.is_empty() {
            return ((0..self.pages).collect(), 0);
        }
        available.sort_unstable_by_key(|x| x.0);
        let (_, index, key) = available[0];
        let mut out = self.page_hier[index].data.pages(key);
        for &(_, index, key) in &available[1..] {
            out = self.page_hier[index].data.intersect_pages(key, &out);
            if out.is_empty() {
                break;
            }
        }
        (out, lookups)
    }

    pub fn candidate_pages(&self, predicates: &[Predicate]) -> Vec<u32> {
        self.candidate_pages_with_lookups(predicates).0
    }

    fn query_from_rows(
        &self,
        rows: &[u32],
        predicates: &[(usize, u64)],
        lookups: u64,
        mut collect: Option<(&mut Vec<u64>, usize)>,
    ) -> QueryStats {
        let mut stats = QueryStats {
            hierarchy_lookups: lookups,
            ..Default::default()
        };
        for segment in &self.segments {
            let end = segment.row_start + segment.data.rows() as u64;
            let lo = rows.partition_point(|&x| (x as u64) < segment.row_start);
            let hi = rows.partition_point(|&x| (x as u64) < end);
            let mut last_page = None;
            for &row_id in &rows[lo..hi] {
                let local = (row_id as u64 - segment.row_start) as usize;
                let page = local / self.page_rows;
                if last_page != Some(page) {
                    stats.pages_touched += 1;
                    last_page = Some(page);
                }
                stats.rows_checked += 1;
                if segment.data.matches(local, predicates) {
                    stats.hits += 1;
                    if let Some((out, limit)) = collect.as_mut() {
                        if out.len() < *limit {
                            out.push(row_id as u64);
                        }
                    }
                }
            }
        }
        stats
    }

    pub fn query(&self, predicates: &[Predicate]) -> QueryStats {
        let pred: Vec<_> = predicates.iter().map(|x| (x.column, x.value)).collect();
        if let Some(plan) = self.candidate_rows_with_lookups(predicates) {
            if plan.fully_covered {
                return QueryStats {
                    hits: plan.rows.len() as u64,
                    rows_checked: 0,
                    pages_touched: 0,
                    hierarchy_lookups: plan.lookups,
                };
            }
            return self.query_from_rows(&plan.rows, &pred, plan.lookups, None);
        }

        let (pages, lookups) = self.candidate_pages_with_lookups(predicates);
        let mut stats = QueryStats {
            hierarchy_lookups: lookups,
            ..Default::default()
        };
        for segment in &self.segments {
            let page_count = ((segment.data.rows() + self.page_rows - 1) / self.page_rows) as u32;
            let lo = pages.partition_point(|&x| x < segment.first_page);
            let hi = pages.partition_point(|&x| x < segment.first_page + page_count);
            for &page_id in &pages[lo..hi] {
                let start = (page_id - segment.first_page) as usize * self.page_rows;
                let end = (start + self.page_rows).min(segment.data.rows());
                stats.rows_checked += (end - start) as u64;
                stats.pages_touched += 1;
                stats.hits += segment.data.count_page(start, end, &pred);
            }
        }
        stats
    }

    pub fn scan(&self, predicates: &[Predicate]) -> QueryStats {
        if self.query_map(predicates).is_none() {
            return Default::default();
        }
        let pred: Vec<_> = predicates.iter().map(|x| (x.column, x.value)).collect();
        let hits = self
            .segments
            .iter()
            .map(|segment| segment.data.count_page(0, segment.data.rows(), &pred))
            .sum();
        QueryStats {
            hits,
            rows_checked: self.rows,
            pages_touched: self.pages as u64,
            hierarchy_lookups: 0,
        }
    }

    pub fn query_count(&self, predicates: &[Predicate]) -> (u64, u64) {
        let stats = self.query(predicates);
        (stats.hits, stats.rows_checked)
    }

    pub fn query_row_ids(
        &self,
        predicates: &[Predicate],
        limit: usize,
    ) -> (Vec<u64>, QueryStats) {
        let pred: Vec<_> = predicates.iter().map(|x| (x.column, x.value)).collect();
        let mut out = Vec::with_capacity(limit.min(1024));
        if let Some(plan) = self.candidate_rows_with_lookups(predicates) {
            if plan.fully_covered {
                out.extend(plan.rows.iter().take(limit).map(|&x| x as u64));
                return (
                    out,
                    QueryStats {
                        hits: plan.rows.len() as u64,
                        rows_checked: 0,
                        pages_touched: 0,
                        hierarchy_lookups: plan.lookups,
                    },
                );
            }
            let stats =
                self.query_from_rows(&plan.rows, &pred, plan.lookups, Some((&mut out, limit)));
            return (out, stats);
        }

        let (pages, lookups) = self.candidate_pages_with_lookups(predicates);
        let mut stats = QueryStats {
            hierarchy_lookups: lookups,
            ..Default::default()
        };
        for segment in &self.segments {
            let page_count = ((segment.data.rows() + self.page_rows - 1) / self.page_rows) as u32;
            let lo = pages.partition_point(|&x| x < segment.first_page);
            let hi = pages.partition_point(|&x| x < segment.first_page + page_count);
            for &page_id in &pages[lo..hi] {
                let start = (page_id - segment.first_page) as usize * self.page_rows;
                let end = (start + self.page_rows).min(segment.data.rows());
                stats.rows_checked += (end - start) as u64;
                stats.pages_touched += 1;
                for row in start..end {
                    if segment.data.matches(row, &pred) {
                        stats.hits += 1;
                        if out.len() < limit {
                            out.push(segment.row_start + row as u64);
                        }
                    }
                }
            }
        }
        (out, stats)
    }

    pub fn row(&self, id: u64) -> Option<Vec<u64>> {
        let segment = self.segments.iter().find(|segment| {
            id >= segment.row_start && id < segment.row_start + segment.data.rows() as u64
        })?;
        let local = (id - segment.row_start) as usize;
        Some(
            (0..self.columns)
                .map(|column| segment.data.value(local, column).unwrap())
                .collect(),
        )
    }
}

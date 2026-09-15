use crate::{mixed_radix_key, BitmapHierarchy, Hierarchy, Manifest, PostingHierarchy, Segment};
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

struct LoadedPageHierarchy {
    columns: Vec<usize>,
    data: PageHierarchyData,
}

struct LoadedRowHierarchy {
    columns: Vec<usize>,
    data: PostingHierarchy,
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
        let m: Manifest = serde_json::from_slice(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if !m.format.starts_with("LHR/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported LHR manifest",
            ));
        }

        let mut page_hier = Vec::new();
        let mut row_hier = Vec::new();
        for h in &m.hierarchies {
            let path = root.join("routing").join(&h.file);
            match h.kind.as_str() {
                "sparse" => page_hier.push(LoadedPageHierarchy {
                    columns: h.columns.clone(),
                    data: PageHierarchyData::Sparse(Hierarchy::open(path)?),
                }),
                "bitmap" => page_hier.push(LoadedPageHierarchy {
                    columns: h.columns.clone(),
                    data: PageHierarchyData::Bitmap(BitmapHierarchy::open(path)?),
                }),
                "postings" => row_hier.push(LoadedRowHierarchy {
                    columns: h.columns.clone(),
                    data: PostingHierarchy::open(path)?,
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
        for s in &m.segments {
            let data = Segment::open(root.join("canonical").join(&s.file))?;
            if data.rows() != s.rows as usize || data.cols() != m.columns {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "segment metadata mismatch",
                ));
            }
            segments.push(LoadedSegment {
                first_page: s.first_page,
                row_start: s.row_start,
                data,
            });
        }

        Ok(Self {
            rows: m.rows,
            page_rows: m.page_rows,
            pages: m.pages,
            columns: m.columns,
            card: m.cardinalities,
            page_hier,
            row_hier,
            segments,
        })
    }

    fn query_map(&self, predicates: &[Predicate]) -> Option<HashMap<usize, u64>> {
        let mut q = HashMap::new();
        for p in predicates {
            if p.column >= self.columns || self.card.get(p.column).map_or(true, |&k| p.value >= k) {
                return None;
            }
            if let Some(old) = q.insert(p.column, p.value) {
                if old != p.value {
                    return None;
                }
            }
        }
        Some(q)
    }

    fn hierarchy_key(&self, columns: &[usize], q: &HashMap<usize, u64>) -> Option<u64> {
        let mut vals = Vec::with_capacity(columns.len());
        for &c in columns {
            vals.push((c, *q.get(&c)?));
        }
        mixed_radix_key(&vals, &self.card)
    }

    /// Plan exact row-posting hierarchies as a weighted set cover over query columns.
    /// Once selected postings cover every predicate column, their intersection is the
    /// exact answer and no canonical row reads are required.
    fn candidate_rows_with_lookups(&self, predicates: &[Predicate]) -> Option<RowPlan> {
        let q = match self.query_map(predicates) {
            Some(q) => q,
            None => {
                return Some(RowPlan {
                    rows: Vec::new(),
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

        let mut applicable = Vec::new();
        for (i, h) in self.row_hier.iter().enumerate() {
            if h.columns.iter().all(|c| q.contains_key(c)) {
                let key = self.hierarchy_key(&h.columns, &q)?;
                let count = h.data.row_count(key);
                if count == 0 {
                    return Some(RowPlan {
                        rows: Vec::new(),
                        lookups: 1,
                        fully_covered: true,
                    });
                }
                applicable.push(Candidate {
                    count,
                    index: i,
                    key,
                    columns: h.columns.clone(),
                });
            }
        }
        if applicable.is_empty() {
            return None;
        }

        let considered = applicable.len() as u64;
        let mut uncovered: HashSet<usize> = q.keys().copied().collect();
        let mut selected: Vec<Candidate> = Vec::new();
        let mut used = vec![false; applicable.len()];

        while !uncovered.is_empty() {
            let mut best: Option<(usize, usize, usize)> = None; // (candidate index, gain, count)
            for (ci, c) in applicable.iter().enumerate() {
                if used[ci] {
                    continue;
                }
                let gain = c.columns.iter().filter(|x| uncovered.contains(x)).count();
                if gain == 0 {
                    continue;
                }
                match best {
                    None => best = Some((ci, gain, c.count)),
                    Some((_, best_gain, best_count)) => {
                        // Minimize count / newly-covered-column without floating point.
                        // Ties prefer the smaller raw posting list.
                        let left = (c.count as u128) * (best_gain as u128);
                        let right = (best_count as u128) * (gain as u128);
                        if left < right || (left == right && c.count < best_count) {
                            best = Some((ci, gain, c.count));
                        }
                    }
                }
            }
            let Some((ci, _, _)) = best else { break };
            used[ci] = true;
            for col in &applicable[ci].columns {
                uncovered.remove(col);
            }
            selected.push(applicable[ci].clone());
        }

        if selected.is_empty() {
            return None;
        }

        // Materialize the smallest selected list first, then stream-intersect other
        // postings directly against the seed list.
        selected.sort_unstable_by_key(|x| x.count);
        let first = &selected[0];
        let mut rows = self.row_hier[first.index].data.rows(first.key);
        for c in &selected[1..] {
            rows = self.row_hier[c.index].data.intersect_rows(c.key, &rows);
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
        let q = match self.query_map(predicates) {
            Some(q) => q,
            None => return (Vec::new(), 0),
        };
        let mut applicable = Vec::new();
        for (i, h) in self.page_hier.iter().enumerate() {
            if h.columns.iter().all(|c| q.contains_key(c)) {
                let Some(key) = self.hierarchy_key(&h.columns, &q) else {
                    return (Vec::new(), 0);
                };
                let count = h.data.page_count(key);
                if count == 0 {
                    return (Vec::new(), 1);
                }
                applicable.push((count, i, key));
            }
        }
        let lookups = applicable.len() as u64;
        if applicable.is_empty() {
            return ((0..self.pages).collect(), 0);
        }
        applicable.sort_unstable_by_key(|x| x.0);
        let (_, i, key) = applicable[0];
        let mut out = self.page_hier[i].data.pages(key);
        for &(_, i, key) in &applicable[1..] {
            out = self.page_hier[i].data.intersect_pages(key, &out);
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
        pred: &[(usize, u64)],
        lookups: u64,
        mut collect: Option<(&mut Vec<u64>, usize)>,
    ) -> QueryStats {
        let mut stats = QueryStats {
            hierarchy_lookups: lookups,
            ..QueryStats::default()
        };
        for s in &self.segments {
            let end_global = s.row_start + s.data.rows() as u64;
            let lo = rows.partition_point(|&x| (x as u64) < s.row_start);
            let hi = rows.partition_point(|&x| (x as u64) < end_global);
            let mut last_page = None;
            for &rid in &rows[lo..hi] {
                let local = (rid as u64 - s.row_start) as usize;
                let page = local / self.page_rows;
                if last_page != Some(page) {
                    stats.pages_touched += 1;
                    last_page = Some(page);
                }
                stats.rows_checked += 1;
                if s.data.matches(local, pred) {
                    stats.hits += 1;
                    if let Some((out, limit)) = collect.as_mut() {
                        if out.len() < *limit {
                            out.push(rid as u64);
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
            ..QueryStats::default()
        };
        for s in &self.segments {
            let page_count = ((s.data.rows() + self.page_rows - 1) / self.page_rows) as u32;
            let lo = pages.partition_point(|&x| x < s.first_page);
            let hi = pages.partition_point(|&x| x < s.first_page + page_count);
            for &pid in &pages[lo..hi] {
                let start = (pid - s.first_page) as usize * self.page_rows;
                let end = (start + self.page_rows).min(s.data.rows());
                stats.rows_checked += (end - start) as u64;
                stats.pages_touched += 1;
                stats.hits += s.data.count_page(start, end, &pred);
            }
        }
        stats
    }

    pub fn scan(&self, predicates: &[Predicate]) -> QueryStats {
        if self.query_map(predicates).is_none() {
            return QueryStats::default();
        }
        let pred: Vec<_> = predicates.iter().map(|x| (x.column, x.value)).collect();
        let mut hits = 0;
        for s in &self.segments {
            hits += s.data.count_page(0, s.data.rows(), &pred);
        }
        QueryStats {
            hits,
            rows_checked: self.rows,
            pages_touched: self.pages as u64,
            hierarchy_lookups: 0,
        }
    }

    pub fn query_count(&self, predicates: &[Predicate]) -> (u64, u64) {
        let s = self.query(predicates);
        (s.hits, s.rows_checked)
    }

    pub fn query_row_ids(&self, predicates: &[Predicate], limit: usize) -> (Vec<u64>, QueryStats) {
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
            let stats = self.query_from_rows(
                &plan.rows,
                &pred,
                plan.lookups,
                Some((&mut out, limit)),
            );
            return (out, stats);
        }

        let (pages, lookups) = self.candidate_pages_with_lookups(predicates);
        let mut stats = QueryStats {
            hierarchy_lookups: lookups,
            ..QueryStats::default()
        };
        for s in &self.segments {
            let page_count = ((s.data.rows() + self.page_rows - 1) / self.page_rows) as u32;
            let lo = pages.partition_point(|&x| x < s.first_page);
            let hi = pages.partition_point(|&x| x < s.first_page + page_count);
            for &pid in &pages[lo..hi] {
                let start = (pid - s.first_page) as usize * self.page_rows;
                let end = (start + self.page_rows).min(s.data.rows());
                stats.rows_checked += (end - start) as u64;
                stats.pages_touched += 1;
                for r in start..end {
                    if s.data.matches(r, &pred) {
                        stats.hits += 1;
                        if out.len() < limit {
                            out.push(s.row_start + r as u64);
                        }
                    }
                }
            }
        }
        (out, stats)
    }

    pub fn row(&self, row_id: u64) -> Option<Vec<u64>> {
        let s = self
            .segments
            .iter()
            .find(|s| row_id >= s.row_start && row_id < s.row_start + s.data.rows() as u64)?;
        let local = (row_id - s.row_start) as usize;
        Some(
            (0..self.columns)
                .map(|c| s.data.value(local, c).unwrap())
                .collect(),
        )
    }
}

use crate::{intersect_sorted, mixed_radix_key, Hierarchy, Manifest, Segment};
use std::{collections::HashMap, fs, io, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Predicate { pub column: usize, pub value: u64 }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub hits: u64,
    pub rows_checked: u64,
    pub pages_touched: u64,
    pub hierarchy_lookups: u64,
}

struct LoadedHierarchy { columns: Vec<usize>, data: Hierarchy }
struct LoadedSegment { first_page: u32, data: Segment }

pub struct Engine {
    page_rows: usize,
    pages: u32,
    columns: usize,
    card: Vec<u64>,
    hier: Vec<LoadedHierarchy>,
    segments: Vec<LoadedSegment>,
}

impl Engine {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let raw = fs::read(root.join("manifest.json"))?;
        let m: Manifest = serde_json::from_slice(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if !m.format.starts_with("LHR/") { return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported LHR manifest")); }
        let mut hier = Vec::new();
        for h in &m.hierarchies {
            hier.push(LoadedHierarchy { columns: h.columns.clone(), data: Hierarchy::open(root.join("routing").join(&h.file))? });
        }
        let mut segments = Vec::new();
        for s in &m.segments {
            let data = Segment::open(root.join("canonical").join(&s.file))?;
            if data.rows() != s.rows as usize || data.cols() != m.columns {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "segment metadata mismatch"));
            }
            segments.push(LoadedSegment { first_page: s.first_page, data });
        }
        Ok(Self { page_rows: m.page_rows, pages: m.pages, columns: m.columns, card: m.cardinalities, hier, segments })
    }

    fn query_map(&self, predicates: &[Predicate]) -> HashMap<usize, u64> {
        predicates.iter().map(|x| (x.column, x.value)).collect()
    }

    fn candidate_pages_with_lookups(&self, predicates: &[Predicate]) -> (Vec<u32>, u64) {
        let q = self.query_map(predicates);
        if q.iter().any(|(&c, &v)| c >= self.columns || self.card.get(c).map_or(true, |&k| v >= k)) {
            return (Vec::new(), 0);
        }
        let mut lists: Vec<Vec<u32>> = Vec::new();
        let mut lookups = 0u64;
        for h in &self.hier {
            if h.columns.iter().all(|c| q.contains_key(c)) {
                let vals: Vec<_> = h.columns.iter().map(|&c| (c, q[&c])).collect();
                let key = match mixed_radix_key(&vals, &self.card) { Some(k) => k, None => return (Vec::new(), lookups) };
                lists.push(h.data.pages(key));
                lookups += 1;
            }
        }
        if lists.is_empty() { return ((0..self.pages).collect(), lookups); }
        lists.sort_by_key(Vec::len);
        let mut out = lists.remove(0);
        for x in lists {
            out = intersect_sorted(&out, &x);
            if out.is_empty() { break; }
        }
        (out, lookups)
    }

    pub fn candidate_pages(&self, predicates: &[Predicate]) -> Vec<u32> {
        self.candidate_pages_with_lookups(predicates).0
    }

    pub fn query(&self, predicates: &[Predicate]) -> QueryStats {
        let (pages, hierarchy_lookups) = self.candidate_pages_with_lookups(predicates);
        let pred: Vec<_> = predicates.iter().map(|x| (x.column, x.value)).collect();
        let mut stats = QueryStats { hierarchy_lookups, ..QueryStats::default() };
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

    pub fn query_count(&self, predicates: &[Predicate]) -> (u64, u64) {
        let s = self.query(predicates);
        (s.hits, s.rows_checked)
    }
}

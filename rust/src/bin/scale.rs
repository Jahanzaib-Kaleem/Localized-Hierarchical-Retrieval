use lhr::{build_u8_batches, BuildConfig, Engine, HierarchySpec, Predicate};
use std::{env, fs, path::{Path, PathBuf}, time::Instant};

const CARDS: [u64; 8] = [8, 12, 6, 16, 10, 20, 8, 14];

fn value(row: u64, col: usize) -> u8 {
    let k = CARDS[col];
    let latent = (row.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) >> 17) & 127;
    let patterned = (latent * (col as u64 + 3) + col as u64 * 5) % k;
    let noise = (row.wrapping_mul(11400714819323198485).rotate_left((col * 7) as u32) + col as u64 * 97) % k;
    if (row.wrapping_mul(31) + col as u64 * 13) % 100 < 72 { patterned as u8 } else { noise as u8 }
}

fn make_batch(start: u64, rows: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows * CARDS.len());
    for r in start..start + rows as u64 { for c in 0..CARDS.len() { out.push(value(r, c)); } }
    out
}

fn exact_count(total: u64, q: &[Predicate]) -> u64 {
    let mut n = 0u64;
    'row: for r in 0..total {
        for p in q { if value(r, p.column) as u64 != p.value { continue 'row; } }
        n += 1;
    }
    n
}

fn recursive_bytes(path: &Path) -> u64 {
    let mut sum = 0u64;
    if let Ok(rd) = fs::read_dir(path) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() { if m.is_dir() { sum += recursive_bytes(&e.path()); } else { sum += m.len(); } }
        }
    }
    sum
}

fn rss_kb(field: &str) -> Option<u64> {
    let text = fs::read_to_string("/proc/self/status").ok()?;
    text.lines().find(|l| l.starts_with(field))?.split_whitespace().nth(1)?.parse().ok()
}

fn percentile(mut x: Vec<f64>, p: f64) -> f64 {
    x.sort_by(|a,b| a.partial_cmp(b).unwrap());
    if x.is_empty() { return 0.0; }
    x[((x.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let rows: u64 = args.get(1).and_then(|x| x.parse().ok()).unwrap_or(1_000_000);
    let batch_rows: usize = args.get(2).and_then(|x| x.parse().ok()).unwrap_or(100_000);
    let queries: usize = args.get(3).and_then(|x| x.parse().ok()).unwrap_or(200);
    let root = args.get(4).map(PathBuf::from).unwrap_or_else(|| env::temp_dir().join(format!("lhr-rust-scale-{}", std::process::id())));
    let _ = fs::remove_dir_all(&root); fs::create_dir_all(&root).unwrap();

    let mut hierarchies = Vec::new();
    for a in 0..CARDS.len() { for b in a+1..CARDS.len() { hierarchies.push(HierarchySpec { columns: vec![a,b] }); } }
    for cols in [[0,1,2],[0,3,4],[1,3,5],[2,4,6],[3,5,7],[0,6,7],[1,4,7],[2,5,6]] { hierarchies.push(HierarchySpec { columns: cols.to_vec() }); }
    let cfg = BuildConfig { columns: CARDS.len(), page_rows: 512, cardinalities: CARDS.to_vec(), hierarchies, max_sort_records: 250_000 };

    let batches = (0..rows).step_by(batch_rows).map(|start| make_batch(start, ((rows-start).min(batch_rows as u64)) as usize));
    let t = Instant::now(); let manifest = build_u8_batches(batches, &root, &cfg).expect("build dataset"); let build_s = t.elapsed().as_secs_f64();
    let engine = Engine::open(&root).expect("open dataset");

    let mut latency_ms = Vec::with_capacity(queries); let mut touched = Vec::with_capacity(queries); let mut pages = Vec::with_capacity(queries); let mut exact_ok = 0usize;
    for i in 0..queries {
        let source = (i as u64 * 7919 + 104729) % rows.max(1);
        let width = 2 + i % 4;
        let mut q = Vec::new();
        for j in 0..width {
            let c = (i + j * 3) % CARDS.len();
            if q.iter().any(|p: &Predicate| p.column == c) { continue; }
            q.push(Predicate { column: c, value: value(source, c) as u64 });
        }
        let q0 = Instant::now(); let s = engine.query(&q); latency_ms.push(q0.elapsed().as_secs_f64()*1000.0); touched.push(s.rows_checked as f64); pages.push(s.pages_touched as f64);
        if i < 10 { exact_ok += (s.hits == exact_count(rows, &q)) as usize; }
    }

    let total_bytes = recursive_bytes(&root); let canonical_bytes: u64 = manifest.segments.iter().map(|s| fs::metadata(root.join("canonical").join(&s.file)).unwrap().len()).sum();
    let med_ms = percentile(latency_ms.clone(), 0.50); let p95_ms = percentile(latency_ms, 0.95);
    let med_touch = percentile(touched.clone(), 0.50); let p95_touch = percentile(touched, 0.95);
    let med_pages = percentile(pages, 0.50);
    println!("{{\"rows\":{},\"pages\":{},\"hierarchies\":{},\"build_s\":{:.3},\"disk_mb\":{:.3},\"canonical_mb\":{:.3},\"index_amplification\":{:.3},\"median_query_ms\":{:.4},\"p95_query_ms\":{:.4},\"median_rows_touched\":{},\"median_pct_touched\":{:.6},\"p95_pct_touched\":{:.6},\"median_pages_touched\":{},\"rss_kb\":{},\"hwm_kb\":{},\"exact\":\"{}/10\"}}",
        rows, manifest.pages, manifest.hierarchies.len(), build_s, total_bytes as f64/1e6, canonical_bytes as f64/1e6, (total_bytes-canonical_bytes) as f64/canonical_bytes.max(1) as f64,
        med_ms, p95_ms, med_touch as u64, 100.0*med_touch/rows.max(1) as f64, 100.0*p95_touch/rows.max(1) as f64, med_pages as u64, rss_kb("VmRSS:").unwrap_or(0), rss_kb("VmHWM:").unwrap_or(0), exact_ok);
}

use lhr::{add_exact_hierarchies, build_u32_batches, BuildConfig, Engine, HierarchySpec, Predicate};
use serde_json::json;
use std::{collections::BTreeMap, env, fs, path::Path, time::Instant};

const CARDS: [u64; 12] = [4, 8, 32, 64, 256, 4_096, 50_000, 500_000, 2_000_000, 10_000_000, 20_000_000, 100_000_000];

fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

fn value(row: u64, col: usize) -> u32 {
    let salt = (col as u64 + 1).wrapping_mul(0xD6E8FEB86659FD93);
    (mix64(row ^ salt) % CARDS[col]) as u32
}

fn make_batch(start: u64, rows: usize) -> Vec<u32> {
    let mut out = Vec::with_capacity(rows * CARDS.len());
    for row in start..start + rows as u64 {
        for col in 0..CARDS.len() {
            out.push(value(row, col));
        }
    }
    out
}

fn recursive_bytes(path: &Path) -> u64 {
    let mut sum = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() { sum += recursive_bytes(&entry.path()); } else { sum += meta.len(); }
            }
        }
    }
    sum
}

fn rss_kb(field: &str) -> Option<u64> {
    let text = fs::read_to_string("/proc/self/status").ok()?;
    text.lines().find(|line| line.starts_with(field))?.split_whitespace().nth(1)?.parse().ok()
}

fn percentile(mut values: Vec<f64>, p: f64) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if values.is_empty() { 0.0 } else { values[((values.len() - 1) as f64 * p).round() as usize] }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let rows: u64 = args.get(1).and_then(|x| x.parse().ok()).unwrap_or(5_000_000);
    let queries: usize = args.get(2).and_then(|x| x.parse().ok()).unwrap_or(90);
    let root = env::temp_dir().join(format!("lhr-lead-scale-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    let cfg = BuildConfig { columns: CARDS.len(), page_rows: 1024, cardinalities: CARDS.to_vec(), hierarchies: vec![], max_sort_records: 250_000 };
    let batches = (0..rows).step_by(100_000).map(|start| make_batch(start, ((rows - start).min(100_000)) as usize));

    let start = Instant::now();
    build_u32_batches(batches, &root, &cfg).expect("build lead-like dataset");
    let mut specs: Vec<_> = (0..CARDS.len()).map(|column| HierarchySpec { columns: vec![column] }).collect();
    specs.extend([
        HierarchySpec { columns: vec![0, 3] },
        HierarchySpec { columns: vec![6, 7] },
        HierarchySpec { columns: vec![8, 9] },
    ]);
    let manifest = add_exact_hierarchies(&root, &specs, 250_000).expect("build lead-like exact indexes");
    let build_s = start.elapsed().as_secs_f64();
    let engine = Engine::open(&root).expect("open lead-like dataset");

    let mut kinds = serde_json::Map::new();
    let mut index_mb = serde_json::Map::new();
    for hierarchy in &manifest.hierarchies {
        let name = hierarchy.columns.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("_");
        kinds.insert(name.clone(), json!(hierarchy.kind));
        let bytes = fs::metadata(root.join("routing").join(&hierarchy.file)).unwrap().len();
        index_mb.insert(name, json!(bytes as f64 / 1e6));
    }

    let patterns: [(&str, &[usize]); 9] = [
        ("low_low", &[0, 1]),
        ("low_pair", &[0, 3]),
        ("low_medium", &[3, 6]),
        ("sparse_pair_6_7", &[6, 7]),
        ("sparse_pair_8_9", &[8, 9]),
        ("high_high", &[10, 11]),
        ("w3", &[0, 4, 7]),
        ("w4", &[1, 5, 8, 11]),
        ("w5", &[2, 6, 9, 10, 11]),
    ];

    let mut latency = Vec::with_capacity(queries);
    let mut exact_ok = 0usize;
    let mut pattern_latency: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    let mut pattern_hits: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for i in 0..queries {
        let source = (i as u64 * 104_729 + 7_919) % rows.max(1);
        let (name, columns) = patterns[i % patterns.len()];
        let query: Vec<_> = columns.iter().map(|&column| Predicate { column, value: value(source, column) as u64 }).collect();
        let q0 = Instant::now();
        let result = engine.query(&query);
        let ms = q0.elapsed().as_secs_f64() * 1000.0;
        latency.push(ms);
        pattern_latency.entry(name).or_default().push(ms);
        pattern_hits.entry(name).or_default().push(result.hits);
        if i < 10 { exact_ok += (result.hits == engine.scan(&query).hits) as usize; }
        assert_eq!(result.rows_checked, 0, "lead-like exact query touched canonical rows");
        assert_eq!(result.pages_touched, 0, "lead-like exact query touched canonical pages");
    }

    let mut pattern_stats = serde_json::Map::new();
    for (name, times) in pattern_latency {
        let hits = pattern_hits.remove(name).unwrap_or_default();
        pattern_stats.insert(name.to_string(), json!({
            "n": times.len(),
            "median_ms": percentile(times.clone(), 0.50),
            "p95_ms": percentile(times, 0.95),
            "median_hits": percentile(hits.iter().map(|&x| x as f64).collect(), 0.50) as u64,
            "max_hits": hits.into_iter().max().unwrap_or(0),
        }));
    }

    let total_bytes = recursive_bytes(&root);
    let canonical_bytes: u64 = manifest.segments.iter().map(|segment| fs::metadata(root.join("canonical").join(&segment.file)).unwrap().len()).sum();

    println!("{}", json!({
        "rows": rows,
        "columns": CARDS.len(),
        "cards": CARDS,
        "hierarchies": manifest.hierarchies.len(),
        "kinds": kinds,
        "index_mb": index_mb,
        "build_s": build_s,
        "disk_mb": total_bytes as f64 / 1e6,
        "canonical_mb": canonical_bytes as f64 / 1e6,
        "index_amplification": (total_bytes - canonical_bytes) as f64 / canonical_bytes.max(1) as f64,
        "median_query_ms": percentile(latency.clone(), 0.50),
        "p95_query_ms": percentile(latency, 0.95),
        "rss_kb": rss_kb("VmRSS:").unwrap_or(0),
        "hwm_kb": rss_kb("VmHWM:").unwrap_or(0),
        "exact": format!("{exact_ok}/10"),
        "patterns": pattern_stats,
    }));
}

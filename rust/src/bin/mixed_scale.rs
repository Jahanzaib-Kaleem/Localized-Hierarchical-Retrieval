use lhr::{add_exact_hierarchies, build_u32_batches, BuildConfig, Engine, HierarchySpec, Predicate};
use serde_json::json;
use std::{env, fs, path::Path, time::Instant};

const CARDS: [u64; 6] = [8, 64, 1_024, 10_000, 100_000, 1_000_000];

fn value(row: u64, col: usize) -> u32 {
    let k = CARDS[col];
    let mixed = row
        .wrapping_mul(6364136223846793005u64.wrapping_add(col as u64 * 2 + 1))
        .wrapping_add(1442695040888963407u64.wrapping_mul(col as u64 + 1));
    let patterned = row
        .wrapping_mul((col as u64 + 3) * 7919)
        .wrapping_add(row >> (col + 1))
        .wrapping_add(col as u64 * 104729);
    ((mixed ^ patterned.rotate_left((col * 9 + 3) as u32)) % k) as u32
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
    let mut sum = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    sum += recursive_bytes(&entry.path());
                } else {
                    sum += meta.len();
                }
            }
        }
    }
    sum
}

fn percentile(mut values: Vec<f64>, p: f64) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if values.is_empty() {
        0.0
    } else {
        values[((values.len() - 1) as f64 * p).round() as usize]
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let rows: u64 = args.get(1).and_then(|x| x.parse().ok()).unwrap_or(1_000_000);
    let queries: usize = args.get(2).and_then(|x| x.parse().ok()).unwrap_or(120);
    let root = env::temp_dir().join(format!("lhr-mixed-scale-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    let cfg = BuildConfig {
        columns: CARDS.len(),
        page_rows: 1024,
        cardinalities: CARDS.to_vec(),
        hierarchies: vec![],
        max_sort_records: 250_000,
    };
    let batches = (0..rows).step_by(100_000).map(|start| {
        make_batch(start, ((rows - start).min(100_000)) as usize)
    });
    let start = Instant::now();
    let mut manifest = build_u32_batches(batches, &root, &cfg).expect("build mixed dataset");
    let specs: Vec<_> = (0..CARDS.len())
        .map(|column| HierarchySpec { columns: vec![column] })
        .collect();
    manifest = add_exact_hierarchies(&root, &specs, 250_000).expect("build adaptive exact indexes");
    let build_s = start.elapsed().as_secs_f64();
    let engine = Engine::open(&root).expect("open mixed dataset");

    let mut kinds = serde_json::Map::new();
    for h in &manifest.hierarchies {
        if h.columns.len() == 1 {
            kinds.insert(h.columns[0].to_string(), json!(h.kind));
        }
    }

    let patterns: [&[usize]; 6] = [
        &[0, 1],
        &[0, 5],
        &[1, 3],
        &[2, 4],
        &[0, 2, 5],
        &[1, 3, 4, 5],
    ];
    let mut latency = Vec::with_capacity(queries);
    let mut exact_ok = 0usize;
    for i in 0..queries {
        let source = (i as u64 * 104729 + 7919) % rows.max(1);
        let columns = patterns[i % patterns.len()];
        let q: Vec<_> = columns
            .iter()
            .map(|&column| Predicate {
                column,
                value: value(source, column) as u64,
            })
            .collect();
        let q0 = Instant::now();
        let result = engine.query(&q);
        latency.push(q0.elapsed().as_secs_f64() * 1000.0);
        if i < 10 {
            exact_ok += (result.hits == engine.scan(&q).hits) as usize;
        }
        assert_eq!(result.rows_checked, 0, "adaptive exact query touched canonical rows");
    }

    let total_bytes = recursive_bytes(&root);
    let canonical_bytes: u64 = manifest
        .segments
        .iter()
        .map(|s| fs::metadata(root.join("canonical").join(&s.file)).unwrap().len())
        .sum();
    println!(
        "{}",
        json!({
            "rows": rows,
            "columns": CARDS.len(),
            "cards": CARDS,
            "kinds": kinds,
            "build_s": build_s,
            "disk_mb": total_bytes as f64 / 1e6,
            "canonical_mb": canonical_bytes as f64 / 1e6,
            "index_amplification": (total_bytes - canonical_bytes) as f64 / canonical_bytes.max(1) as f64,
            "median_query_ms": percentile(latency.clone(), 0.50),
            "p95_query_ms": percentile(latency, 0.95),
            "exact": format!("{exact_ok}/10")
        })
    );
}

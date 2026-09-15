use lhr::{add_exact_hierarchies, build_u8_batches, BuildConfig, Engine, HierarchySpec, Predicate};
use serde_json::json;
use std::{collections::BTreeMap, env, fs, time::Instant};

const CARDS: [u64; 8] = [8, 12, 6, 16, 10, 20, 8, 14];
const EDGES: [(usize, usize); 7] = [(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (5, 6), (6, 7)];

fn value(row: u64, col: usize) -> u8 {
    let k = CARDS[col];
    let latent = (row
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
        >> 17)
        & 127;
    let patterned = (latent * (col as u64 + 3) + col as u64 * 5) % k;
    let noise = (row
        .wrapping_mul(11400714819323198485)
        .rotate_left((col * 7) as u32)
        + col as u64 * 97)
        % k;
    if (row.wrapping_mul(31) + col as u64 * 13) % 100 < 72 {
        patterned as u8
    } else {
        noise as u8
    }
}

fn make_batch(start: u64, rows: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows * CARDS.len());
    for row in start..start + rows as u64 {
        for col in 0..CARDS.len() {
            out.push(value(row, col));
        }
    }
    out
}

fn hybrid7_specs() -> Vec<HierarchySpec> {
    let mut out: Vec<_> = (0..CARDS.len())
        .map(|column| HierarchySpec { columns: vec![column] })
        .collect();
    out.extend(
        EDGES
            .iter()
            .map(|&(a, b)| HierarchySpec { columns: vec![a, b] }),
    );
    out
}

fn combinations(width: usize) -> Vec<Vec<usize>> {
    fn visit(start: usize, remaining: usize, current: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if remaining == 0 {
            out.push(current.clone());
            return;
        }
        for col in start..=CARDS.len() - remaining {
            current.push(col);
            visit(col + 1, remaining - 1, current, out);
            current.pop();
        }
    }
    let mut out = Vec::new();
    visit(0, width, &mut Vec::new(), &mut out);
    out
}

fn route_type(columns: &[usize]) -> &'static str {
    let pair_hits = EDGES
        .iter()
        .filter(|&&(a, b)| columns.contains(&a) && columns.contains(&b))
        .count();
    match (columns.len(), pair_hits) {
        (2, 1..) => "direct_pair",
        (_, 1..) => "pair_composed",
        _ => "singleton_only",
    }
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
    let rounds: usize = args.get(2).and_then(|x| x.parse().ok()).unwrap_or(1);

    let root = env::temp_dir().join(format!("lhr-topology-{}", std::process::id()));
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
    build_u8_batches(batches, &root, &cfg).expect("build dataset");
    add_exact_hierarchies(&root, &hybrid7_specs(), 250_000).expect("build exact hierarchies");
    let engine = Engine::open(&root).expect("open dataset");

    let mut buckets: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut exact_checked = 0usize;
    let mut exact_ok = 0usize;
    let mut query_index = 0u64;

    for round in 0..rounds {
        for width in 2..=5 {
            for columns in combinations(width) {
                let source = (query_index
                    .wrapping_mul(7919)
                    .wrapping_add(104729)
                    .wrapping_add(round as u64 * 65537))
                    % rows.max(1);
                query_index += 1;
                let query: Vec<_> = columns
                    .iter()
                    .map(|&column| Predicate {
                        column,
                        value: value(source, column) as u64,
                    })
                    .collect();

                let t0 = Instant::now();
                let routed = engine.query(&query);
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                let route = route_type(&columns);
                buckets
                    .entry(format!("w{width}_{route}"))
                    .or_default()
                    .push(ms);

                if exact_checked < 24 {
                    exact_ok += (routed.hits == engine.scan(&query).hits) as usize;
                    exact_checked += 1;
                }
            }
        }
    }

    let mut routes = serde_json::Map::new();
    for (name, values) in buckets {
        routes.insert(
            name,
            json!({
                "n": values.len(),
                "median_ms": percentile(values.clone(), 0.50),
                "p95_ms": percentile(values, 0.95)
            }),
        );
    }

    println!(
        "{}",
        json!({
            "rows": rows,
            "rounds": rounds,
            "queries": query_index,
            "exact": format!("{exact_ok}/{exact_checked}"),
            "routes": routes
        })
    );
}

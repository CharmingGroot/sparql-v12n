// SPDX-License-Identifier: Apache-2.0
//! Translation time per benchmark query, without a database: parsing and
//! unfolding as `/sparql` does for every request, and rendering the SQL.
//!
//! `cargo bench -p v12n-server --bench translate [-- --json]` measures
//! bench/queries/*.rq over bench/mdr/mapping.ttl. Without `--bench` (as
//! `cargo test --all-targets` runs it) each query is translated once.
//!
//! A plain timing loop instead of a benchmark framework: the numbers that
//! matter are the median and p95 in microseconds next to request times in
//! milliseconds, so a framework's statistics would add dependencies without
//! changing a conclusion.

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use v12n_core::mapping::{Mapping, MappingIndex};
use v12n_core::{sparql, unfold};

/// Measured iterations stop at whichever comes first.
const MAX_ITERATIONS: usize = 20_000;
const BUDGET: Duration = Duration::from_millis(500);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let measure = args.iter().any(|a| a == "--bench");
    let json = args.iter().any(|a| a == "--json");
    let bench = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench");
    let text = std::fs::read_to_string(bench.join("mdr/mapping.ttl")).expect("bench mapping");
    let mapping = Mapping::from_turtle(&text).expect("valid mapping");
    let index = MappingIndex::new(&mapping);
    let mut queries: Vec<_> = std::fs::read_dir(bench.join("queries"))
        .expect("bench/queries")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "rq"))
        .collect();
    queries.sort();

    for path in queries {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let query = std::fs::read_to_string(&path).expect("query file");
        let translate = || {
            let parsed = sparql::parse(&query).expect("query in the supported subset");
            unfold::unfold(&parsed, &mapping, &index).expect("query unfolds")
        };
        let plan = translate();
        if !measure {
            black_box(plan.to_sql().expect("query renders"));
            continue;
        }
        // Warm-up, so that the first allocations do not count.
        for _ in 0..100 {
            black_box(translate().to_sql().expect("query renders"));
        }
        let mut translating = Vec::new();
        let mut rendering = Vec::new();
        let started = Instant::now();
        while translating.len() < MAX_ITERATIONS && started.elapsed() < BUDGET {
            let t0 = Instant::now();
            let plan = black_box(translate());
            let t1 = Instant::now();
            black_box(plan.to_sql().expect("query renders"));
            let t2 = Instant::now();
            translating.push(t1 - t0);
            rendering.push(t2 - t1);
        }
        let (t, r) = (Stats::of(&mut translating), Stats::of(&mut rendering));
        if json {
            println!(
                "{{\"query\":\"{name}\",\"iterations\":{},\"translate_us\":{},\"render_us\":{}}}",
                translating.len(),
                t.json(),
                r.json()
            );
        } else {
            println!(
                "{name:<24} parse+unfold {:>8.1} µs (p95 {:>8.1})   to_sql {:>8.1} µs (p95 {:>8.1})",
                t.median, t.p95, r.median, r.p95
            );
        }
    }
}

struct Stats {
    median: f64,
    p95: f64,
}

impl Stats {
    /// Nearest-rank median and p95, in microseconds.
    fn of(samples: &mut [Duration]) -> Self {
        samples.sort_unstable();
        let rank = |p: f64| {
            let k = (p * samples.len() as f64).ceil() as usize;
            samples[k.saturating_sub(1)].as_secs_f64() * 1e6
        };
        Self {
            median: rank(0.5),
            p95: rank(0.95),
        }
    }

    fn json(&self) -> String {
        format!("{{\"median\":{:.1},\"p95\":{:.1}}}", self.median, self.p95)
    }
}

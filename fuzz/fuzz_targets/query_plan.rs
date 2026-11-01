// SPDX-License-Identifier: Apache-2.0
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use v12n_core::Planner;
use v12n_core::mapping::Mapping;

// The path of `v12n explain`: parse, unfold and render, with no database.
// The mappings cover several triples maps per predicate (UNION branches),
// OPTIONAL parts over one table, references between tables and typed
// columns.
const MAPPINGS: [&str; 4] = [
    include_str!("../../examples/shop/mapping.ttl"),
    include_str!("../../tests/diff/0004-union-column-types/mapping.ttl"),
    include_str!("../../tests/diff/0015-foreign-keys/mapping.ttl"),
    include_str!("../../tests/diff/0016-optional-self-joins/mapping.ttl"),
];

fn planners() -> &'static [Planner] {
    static PLANNERS: OnceLock<Vec<Planner>> = OnceLock::new();
    PLANNERS.get_or_init(|| {
        MAPPINGS
            .iter()
            .map(|turtle| Planner::new(Mapping::from_turtle(turtle).expect("a test mapping")))
            .collect()
    })
}

fuzz_target!(|query: &str| {
    for planner in planners() {
        if let Ok(plan) = planner.plan(query) {
            let _ = plan.to_sql();
        }
    }
});

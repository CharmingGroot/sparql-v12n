// SPDX-License-Identifier: Apache-2.0
#![no_main]

use libfuzzer_sys::fuzz_target;
use v12n_core::Planner;
use v12n_core::mapping::Mapping;

// A mapping that parses also builds the index and catalog request, as
// `v12n check` and every front end do.
fuzz_target!(|turtle: &str| {
    if let Ok(mapping) = Mapping::from_turtle(turtle) {
        let _ = Planner::new(mapping);
    }
});

// SPDX-License-Identifier: Apache-2.0
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|query: &str| {
    let _ = v12n_core::sparql::parse(query);
});

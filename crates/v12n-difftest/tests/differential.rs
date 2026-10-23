// SPDX-License-Identifier: Apache-2.0
//! One test per query of every case under `examples/` and `tests/diff/`.
//! Queries marked `rejected` always run; comparisons with Ontop are
//! skipped unless `V12N_DIFF_DATABASE_URL` and `V12N_DIFF_ENDPOINTS` are
//! set, which `testenv/difftest.sh` does.

include!(concat!(env!("OUT_DIR"), "/cases.rs"));

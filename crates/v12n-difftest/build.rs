// SPDX-License-Identifier: Apache-2.0
//! Generates one test per query of every differential case, so that test
//! filters, `nextest --partition` and JUnit reports work per query.
//!
//! A case is a directory under `examples/` or `tests/diff/` with a
//! `mapping.ttl`; its queries are `queries/*.rq`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

const CASE_ROOTS: [&str; 2] = ["examples", "tests/diff"];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let workspace = manifest
        .join("../..")
        .canonicalize()
        .expect("workspace root");

    let mut out = String::new();
    let mut modules = BTreeSet::new();
    for root in CASE_ROOTS {
        let root = workspace.join(root);
        rerun_if_changed(&root);
        for case in sorted_entries(&root).filter(|p| p.join("mapping.ttl").is_file()) {
            let name = file_name(&case);
            let module = format!("case_{}", ident(&name));
            assert!(modules.insert(module.clone()), "two cases are named {name}");
            let queries = case.join("queries");
            rerun_if_changed(&case);
            rerun_if_changed(&queries);

            writeln!(out, "mod {module} {{").unwrap();
            for query in
                sorted_entries(&queries).filter(|p| p.extension().is_some_and(|e| e == "rq"))
            {
                let query = file_name(&query);
                let query = query.strip_suffix(".rq").expect("filtered on .rq");
                writeln!(
                    out,
                    "    #[test]\n    fn {}() {{\n        v12n_difftest::check({:?}, {query:?});\n    }}",
                    ident(query),
                    case.display().to_string(),
                )
                .unwrap();
            }
            writeln!(out, "}}").unwrap();
        }
    }
    let path = PathBuf::from(env::var("OUT_DIR").expect("set by cargo")).join("cases.rs");
    fs::write(path, out).expect("write generated tests");
}

fn rerun_if_changed(path: &Path) {
    println!("cargo::rerun-if-changed={}", path.display());
}

fn sorted_entries(dir: &Path) -> impl Iterator<Item = PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    entries.sort();
    entries.into_iter()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| panic!("non-UTF-8 name: {}", path.display()))
        .to_owned()
}

/// `0001-mdr` becomes `0001_mdr`: Rust identifiers allow no `-`.
fn ident(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

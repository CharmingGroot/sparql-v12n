// SPDX-License-Identifier: Apache-2.0
//! Records the commit the binary is built from, for `v12n --version`.

use std::process::Command;

fn main() {
    // Release builds set it from the workflow; elsewhere git is asked, and
    // a source tree without git (an unpacked archive) reports "unknown".
    println!("cargo::rerun-if-env-changed=V12N_COMMIT");
    println!("cargo::rerun-if-changed=../../.git/HEAD");
    println!("cargo::rerun-if-changed=../../.git/refs/heads");
    let commit = std::env::var("V12N_COMMIT")
        .ok()
        .filter(|commit| !commit.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|commit| commit.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo::rustc-env=V12N_COMMIT={commit}");
}

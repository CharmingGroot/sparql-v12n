// SPDX-License-Identifier: Apache-2.0
//! Case directories: `schema.sql`, `data.sql` and `mapping.ttl`, or a
//! `case.toml` that points at SQL and mapping files kept elsewhere (the
//! vendored W3C test cases, which must stay unmodified); then
//! `queries/*.rq` and an optional `expect.toml`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Error;

/// One case: a mapping over one database, and the queries to compare.
#[derive(Debug, Clone)]
pub struct Case {
    /// The directory name, such as `shop` or `0001-mdr`.
    pub name: String,
    pub dir: PathBuf,
    pub mapping: String,
    expectations: BTreeMap<String, Expectation>,
}

/// What a query of a case is expected to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// Same solutions as Ontop. The default.
    Match,
    /// Outside the supported subset: the engine must fail with
    /// `UnsupportedFeature` instead of answering. Ontop is not asked.
    Rejected,
    /// A known difference from Ontop, tracked in an issue. The check fails
    /// once the results match, so the expectation cannot go stale.
    Mismatch { issue: u32 },
    /// Ontop fails on the query, so there is nothing to compare with. The
    /// engine must still answer with this many solutions, and the check
    /// fails once Ontop answers.
    OntopError { issue: u32, solutions: usize },
}

/// One query file of a case.
#[derive(Debug, Clone)]
pub struct QueryCase {
    /// The file name without `.rq`.
    pub name: String,
    pub text: String,
    pub expect: Expectation,
}

/// `case.toml`: paths relative to the case directory. One string per key,
/// since `testenv/difftest.sh` reads them with `sed`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    sql: String,
    mapping: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectFile {
    #[serde(default)]
    query: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    expect: Kind,
    issue: Option<u32>,
    solutions: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Rejected,
    Mismatch,
    #[serde(rename = "ontop-error")]
    OntopError,
}

impl Case {
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| Error::case(dir, "case directory has no UTF-8 name"))?
            .to_owned();
        let case_path = dir.join("case.toml");
        let mapping = if case_path.is_file() {
            let file: CaseFile =
                toml::from_str(&read(&case_path)?).map_err(|e| Error::case(&case_path, e))?;
            let sql = dir.join(&file.sql);
            if !sql.is_file() {
                return Err(Error::case(
                    &case_path,
                    format!("no SQL file {}", sql.display()),
                ));
            }
            read(&dir.join(&file.mapping))?
        } else {
            read(&dir.join("mapping.ttl"))?
        };
        let queries = query_names(dir)?;

        let expect_path = dir.join("expect.toml");
        let mut expectations = BTreeMap::new();
        if expect_path.is_file() {
            let file: ExpectFile =
                toml::from_str(&read(&expect_path)?).map_err(|e| Error::case(&expect_path, e))?;
            for (query, entry) in file.query {
                if !queries.contains(&query) {
                    return Err(Error::case(
                        &expect_path,
                        format!("no query {query}.rq for this expectation"),
                    ));
                }
                let invalid =
                    |message: &str| Error::case(&expect_path, format!("{query}: {message}"));
                let expectation = match (entry.expect, entry.issue, entry.solutions) {
                    (Kind::Rejected, None, None) => Expectation::Rejected,
                    (Kind::Mismatch, Some(issue), None) => Expectation::Mismatch { issue },
                    (Kind::OntopError, Some(issue), Some(solutions)) => {
                        Expectation::OntopError { issue, solutions }
                    }
                    (Kind::Rejected, ..) => return Err(invalid("rejected takes no other keys")),
                    (Kind::Mismatch | Kind::OntopError, None, _) => {
                        return Err(invalid("name the issue that tracks it"));
                    }
                    (Kind::Mismatch, _, Some(_)) => {
                        return Err(invalid("mismatch takes no solutions"));
                    }
                    (Kind::OntopError, _, None) => {
                        return Err(invalid("give the number of solutions v12n must return"));
                    }
                };
                expectations.insert(query, expectation);
            }
        }
        Ok(Self {
            name,
            dir: dir.to_owned(),
            mapping,
            expectations,
        })
    }

    pub fn query(&self, name: &str) -> Result<QueryCase, Error> {
        let path = self.dir.join("queries").join(format!("{name}.rq"));
        Ok(QueryCase {
            name: name.to_owned(),
            text: read(&path)?,
            expect: self
                .expectations
                .get(name)
                .copied()
                .unwrap_or(Expectation::Match),
        })
    }

    /// The PostgreSQL database this case is loaded into. `testenv/difftest.sh`
    /// uses the same rule.
    pub fn database(&self) -> String {
        format!("diff_{}", self.name.to_ascii_lowercase().replace('-', "_"))
    }
}

/// Names of `queries/*.rq`, without the extension.
fn query_names(dir: &Path) -> Result<Vec<String>, Error> {
    let queries = dir.join("queries");
    let entries = fs::read_dir(&queries).map_err(|e| Error::case(&queries, e))?;
    let mut names = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| Error::case(&queries, e))?.path();
        if path.extension().is_some_and(|e| e == "rq")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            names.push(stem.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn read(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(|e| Error::case(path, e))
}

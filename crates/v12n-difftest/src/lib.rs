// SPDX-License-Identifier: Apache-2.0
//! Differential tests: every query of a case runs through this engine and
//! through Ontop over the same mapping and data, and the two results must
//! be the same bag of solutions.

mod case;
mod compare;
mod engine;
mod error;
mod ontop;
mod order;

use std::path::Path;

pub use case::{Case, Expectation, QueryCase};
pub use compare::{Diff, Results, Row, compare};
pub use error::Error;
pub use order::Order;

/// Base PostgreSQL URL. Each case is loaded into its own database, named
/// by [`Case::database`], which replaces the database in this URL.
pub const DATABASE_URL_VAR: &str = "V12N_DIFF_DATABASE_URL";
/// Path of a file with one `<case> <Ontop SPARQL endpoint URL>` per line;
/// the URL is `-` when Ontop did not start with the case's mapping.
pub const ENDPOINTS_VAR: &str = "V12N_DIFF_ENDPOINTS";
/// Optional path of a file that gets one line per retried Ontop request.
pub const RETRY_LOG_VAR: &str = "V12N_DIFF_RETRY_LOG";

/// What a passing check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Both engines returned the same solutions.
    Matched { solutions: usize },
    /// The engine rejected a query outside the supported subset.
    Rejected { feature: String },
    /// The results differ as recorded in the case's expectations.
    KnownMismatch { issue: u32, diff: String },
    /// Ontop failed as recorded, and the engine returned the expected
    /// number of solutions.
    KnownOntopError { issue: u32, error: String },
    /// No database or Ontop endpoint is configured.
    Skipped,
}

/// Runs one query of the case in `case_dir` and panics with a report if
/// the check fails. Used by the generated tests.
pub fn check(case_dir: &str, query: &str) {
    match run(Path::new(case_dir), query) {
        Ok(Outcome::Matched { solutions }) => eprintln!("same as Ontop: {solutions} solutions"),
        Ok(Outcome::Rejected { feature }) => eprintln!("rejected: unsupported feature: {feature}"),
        Ok(Outcome::KnownMismatch { issue, diff }) => {
            eprintln!("known mismatch, tracked in #{issue}:\n{diff}");
        }
        Ok(Outcome::KnownOntopError { issue, error }) => {
            eprintln!("known Ontop error, tracked in #{issue}: {error}");
        }
        Ok(Outcome::Skipped) => {
            eprintln!("skipped: {DATABASE_URL_VAR} and {ENDPOINTS_VAR} are not set");
        }
        Err(error) => panic!("{error}"),
    }
}

/// Checks that the mapping of the case in `case_dir`, whose `case.toml`
/// has `expect = "mapping-error"`, does not load, and panics with a report
/// otherwise. Neither a database nor Ontop is needed. Used by the
/// generated tests.
pub fn check_mapping_error(case_dir: &str) {
    match run_mapping_error(Path::new(case_dir)) {
        Ok(error) => eprintln!("mapping rejected: {error}"),
        Err(error) => panic!("{error}"),
    }
}

/// The error that loading the mapping of the case in `case_dir` gives.
pub fn run_mapping_error(case_dir: &Path) -> Result<String, Error> {
    let case = Case::load(case_dir)?;
    if !case.expects_mapping_error {
        return Err(Error::case(
            case_dir.join("case.toml"),
            "the case does not expect a mapping error",
        ));
    }
    engine::mapping_error(&case.mapping).map_err(|e| Error::Failed(format!("{}: {e}", case.name)))
}

/// Runs one query of the case in `case_dir`. Queries that planning
/// rejects need neither a database nor Ontop.
pub fn run(case_dir: &Path, query: &str) -> Result<Outcome, Error> {
    let case = Case::load(case_dir)?;
    let query = case.query(query)?;
    let label = format!("{}/{}", case.name, query.name);

    let failed = |message: String| Error::Failed(format!("{label}: {message}"));
    if query.expect == Expectation::Rejected
        && let Some(feature) =
            engine::rejected_at_planning(&case.mapping, case.base_iri.as_deref(), &query.text)
                .map_err(failed)?
    {
        return Ok(Outcome::Rejected { feature });
    }

    let Some(environment) = Environment::from_env()? else {
        return Ok(Outcome::Skipped);
    };
    let database_url = with_database(&environment.database_url, &case.database())?;

    if query.expect == Expectation::Rejected {
        // Some checks need the column types, so they reject at execution.
        return match engine::run(
            &database_url,
            &case.mapping,
            case.base_iri.as_deref(),
            &query.text,
        ) {
            Err(engine::Failure::Unsupported(feature)) => Ok(Outcome::Rejected { feature }),
            Err(engine::Failure::Other(e)) => Err(failed(format!(
                "expected an unsupported-feature error, got: {e}"
            ))),
            Ok(_) => Err(failed(
                "expected an unsupported-feature error, but the engine answers; remove \
                 `expect = \"rejected\"` from expect.toml so it is compared with Ontop"
                    .to_owned(),
            )),
        };
    }

    let endpoint = environment.endpoint(&case.name)?;
    // "-": Ontop refused the mapping at startup (see testenv/difftest.sh).
    let expected = if endpoint == "-" {
        Err("Ontop did not start with this mapping; its container log has the cause".to_owned())
    } else {
        ontop::query(&endpoint, &label, &query.text)
    };
    let actual = engine::run(
        &database_url,
        &case.mapping,
        case.base_iri.as_deref(),
        &query.text,
    );

    match query.expect {
        Expectation::OntopError { issue, solutions } => {
            let actual = actual.map_err(|e| Error::Engine(format!("{label}: {e}")))?;
            match expected {
                Ok(_) => Err(Error::Failed(format!(
                    "{label}: Ontop answers now; remove its expectation from expect.toml \
                     so the results are compared, and update #{issue}"
                ))),
                Err(_) if actual.rows.len() != solutions => Err(Error::Failed(format!(
                    "{label}: v12n returned {} solutions, expected {solutions}",
                    actual.rows.len()
                ))),
                Err(error) => Ok(Outcome::KnownOntopError { issue, error }),
            }
        }
        Expectation::Mismatch { issue } => {
            let expected = expected.map_err(|e| Error::Ontop(format!("{label}: {e}")))?;
            // A known mismatch may also be v12n failing where Ontop answers.
            let diff = match actual {
                Ok(actual) => diff(&label, &query.text, &expected, &actual)?.map(|d| d.to_string()),
                Err(error) => Some(format!("  v12n fails: {error}\n")),
            };
            match diff {
                Some(diff) => Ok(Outcome::KnownMismatch { issue, diff }),
                None => Err(Error::Failed(format!(
                    "{label}: matches Ontop now; remove its expectation from expect.toml \
                     and close #{issue}"
                ))),
            }
        }
        Expectation::Match => {
            let expected = expected.map_err(|e| Error::Ontop(format!("{label}: {e}")))?;
            let actual = actual.map_err(|e| Error::Engine(format!("{label}: {e}")))?;
            match diff(&label, &query.text, &expected, &actual)? {
                None => Ok(Outcome::Matched {
                    solutions: actual.rows.len(),
                }),
                Some(diff) => Err(Error::Failed(format!(
                    "{label}: results differ from Ontop\n{diff}query:\n{}",
                    query.text
                ))),
            }
        }
        Expectation::Rejected => unreachable!("handled above"),
    }
}

fn diff(
    label: &str,
    query: &str,
    expected: &Results,
    actual: &Results,
) -> Result<Option<Diff>, Error> {
    let failed = |e: String| Error::Failed(format!("{label}: {e}"));
    let order = Order::of(query).map_err(failed)?;
    compare(expected, actual, &order).map_err(failed)
}

struct Environment {
    database_url: String,
    endpoints: String,
    endpoints_path: String,
}

impl Environment {
    /// `None` when neither variable is set, so that plain `cargo test`
    /// skips the comparisons. Setting only one of them is an error, since
    /// that is a broken setup rather than an opt-out.
    fn from_env() -> Result<Option<Self>, Error> {
        let database_url = std::env::var(DATABASE_URL_VAR).ok();
        let endpoints_path = std::env::var(ENDPOINTS_VAR).ok();
        let (database_url, endpoints_path) = match (database_url, endpoints_path) {
            (None, None) => return Ok(None),
            (Some(d), Some(e)) => (d, e),
            _ => {
                return Err(Error::Environment(format!(
                    "set both {DATABASE_URL_VAR} and {ENDPOINTS_VAR}, or neither"
                )));
            }
        };
        let endpoints = std::fs::read_to_string(&endpoints_path)
            .map_err(|e| Error::Environment(format!("{ENDPOINTS_VAR}={endpoints_path}: {e}")))?;
        Ok(Some(Self {
            database_url,
            endpoints,
            endpoints_path,
        }))
    }

    fn endpoint(&self, case: &str) -> Result<String, Error> {
        self.endpoints
            .lines()
            .filter_map(|line| line.split_once(' '))
            .find(|(name, _)| *name == case)
            .map(|(_, url)| url.trim().to_owned())
            .ok_or_else(|| {
                Error::Environment(format!(
                    "no Ontop endpoint for case {case} in {}",
                    self.endpoints_path
                ))
            })
    }
}

/// Replaces the database of a `postgresql://` URL, keeping user, host,
/// port and query parameters.
fn with_database(url: &str, database: &str) -> Result<String, Error> {
    let invalid = || Error::Environment(format!("{DATABASE_URL_VAR} must be a postgresql:// URL"));
    let rest = url
        .strip_prefix("postgresql://")
        .or_else(|| url.strip_prefix("postgres://"))
        .ok_or_else(invalid)?;
    let scheme = &url[..url.len() - rest.len()];
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let query = tail.find('?').map_or("", |i| &tail[i..]);
    Ok(format!("{scheme}{authority}/{database}{query}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_database_replaces_only_the_database() {
        let cases = [
            ("postgresql://u:p@h:1/shop", "postgresql://u:p@h:1/diff_x"),
            ("postgresql://u:p@h:1", "postgresql://u:p@h:1/diff_x"),
            (
                "postgres://u@h/shop?connect_timeout=5",
                "postgres://u@h/diff_x?connect_timeout=5",
            ),
            (
                "postgresql://h?sslmode=disable",
                "postgresql://h/diff_x?sslmode=disable",
            ),
        ];
        for (url, expected) in cases {
            assert_eq!(with_database(url, "diff_x").unwrap(), expected, "{url}");
        }
        assert!(with_database("host=h dbname=shop", "diff_x").is_err());
    }

    #[test]
    fn mapping_error_needs_an_invalid_mapping() {
        let mapping = |subject_map: &str| {
            format!(
                "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
                 <http://ex/A> rr:logicalTable [ rr:tableName \"a\" ] ;\n\
                 {subject_map}\n\
                 rr:predicateObjectMap [ rr:predicate <http://ex/p> ; rr:objectMap [ rr:column \"x\" ] ] ."
            )
        };
        let valid = mapping("rr:subjectMap [ rr:template \"http://ex/{id}\" ] ;");
        let unsupported = mapping(
            "rr:subjectMap [ rr:template \"http://ex/{id}\" ; rr:graphMap [ rr:constant <http://ex/g> ] ] ;",
        );
        let invalid = mapping("");

        let loads = engine::mapping_error(&valid).unwrap_err();
        assert!(loads.contains("the mapping loads"), "{loads}");
        let unsupported = engine::mapping_error(&unsupported).unwrap_err();
        assert!(unsupported.contains("unsupported"), "{unsupported}");
        let invalid = engine::mapping_error(&invalid).unwrap();
        assert!(invalid.contains("has no rr:subjectMap"), "{invalid}");
    }
}

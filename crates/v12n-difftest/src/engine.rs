// SPDX-License-Identifier: Apache-2.0
//! Runs a query through this engine with library calls, as `v12n query`
//! does, without the CLI or HTTP layers.

use v12n_core::mapping::{BaseIri, Mapping, MappingIndex};
use v12n_core::sql::SqlPlan;
use v12n_core::{sparql, unfold};
use v12n_postgres::{Database, Limits};

use crate::compare::{Results, Row};

/// Unfolds the query; the outer error is a broken mapping, which is a
/// problem with the case rather than with the query.
fn unfold_query(
    mapping: &str,
    base_iri: Option<&str>,
    query: &str,
) -> Result<Result<SqlPlan, v12n_core::Error>, String> {
    let mut mapping = Mapping::from_turtle(mapping).map_err(|e| format!("mapping.ttl: {e}"))?;
    if let Some(base) = base_iri {
        let base = BaseIri::new(base).map_err(|e| format!("case.toml: {e}"))?;
        mapping = mapping.with_base_iri(base);
    }
    Ok(sparql::parse(query)
        .and_then(|query| unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping))))
}

/// Why the engine did not answer.
#[derive(Debug)]
pub enum Failure {
    /// `UnsupportedFeature`, at planning or, for checks that need the
    /// column types, at execution. The CLI turns it into exit code 1 and
    /// the HTTP endpoint into status 501.
    Unsupported(String),
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(feature) => write!(
                f,
                "unsupported feature: {feature}; if this is intended, mark the query \
                 `expect = \"rejected\"` in expect.toml"
            ),
            Self::Other(message) => f.write_str(message),
        }
    }
}

/// `Some(feature)` when planning rejects the query as unsupported, `None`
/// when it plans, and an error for any other failure.
pub fn rejected_at_planning(
    mapping: &str,
    base_iri: Option<&str>,
    query: &str,
) -> Result<Option<String>, String> {
    match unfold_query(mapping, base_iri, query)? {
        Err(v12n_core::Error::UnsupportedFeature(feature)) => Ok(Some(feature)),
        Err(e) => Err(format!("expected an unsupported-feature error, got: {e}")),
        Ok(_) => Ok(None),
    }
}

/// All solutions, with no row limit so that a cut-off result cannot pass
/// for a complete one.
pub fn run(
    database_url: &str,
    mapping: &str,
    base_iri: Option<&str>,
    query: &str,
) -> Result<Results, Failure> {
    let plan = unfold_query(mapping, base_iri, query)
        .map_err(Failure::Other)?
        .map_err(|e| match e {
            v12n_core::Error::UnsupportedFeature(feature) => Failure::Unsupported(feature),
            e => Failure::Other(e.to_string()),
        })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::Other(e.to_string()))?;
    runtime
        .block_on(async {
            let limits = Limits {
                row_limit: 0,
                ..Limits::default()
            };
            let database = Database::connect(database_url, limits).await?;
            let mut solutions = database.execute(&plan).await?;
            let variables = solutions.variables().to_vec();
            let mut rows = Vec::new();
            while let Some(solution) = solutions.try_next().await? {
                let row: Row = variables
                    .iter()
                    .zip(solution.values())
                    .filter_map(|(v, t)| t.clone().map(|t| (v.clone(), t)))
                    .collect();
                rows.push(row);
            }
            Ok::<_, v12n_postgres::Error>(Results { variables, rows })
        })
        .map_err(|e| match e {
            v12n_postgres::Error::Core(v12n_core::Error::UnsupportedFeature(feature)) => {
                Failure::Unsupported(feature)
            }
            e => Failure::Other(e.to_string()),
        })
}

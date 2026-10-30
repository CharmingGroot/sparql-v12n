// SPDX-License-Identifier: Apache-2.0
//! Runs a query through this engine with library calls, as `v12n query`
//! does, without the CLI or HTTP layers.

use std::collections::HashMap;
use std::sync::Arc;

use v12n_core::backend::{BackendError, Connector, ErrorKind, SolutionStream};
use v12n_core::mapping::{BaseIri, Mapping};
use v12n_core::sql::SqlPlan;
use v12n_core::{Engine, Limits, Planner};
use v12n_postgres::{PostgresBackend, PostgresConnector};

use crate::compare::{Results, Row};

/// The case's mapping, ready to plan; an error is a broken mapping, which
/// is a problem with the case rather than with the query.
fn planner(mapping: &str, base_iri: Option<&str>) -> Result<Planner, String> {
    let mut mapping = Mapping::from_turtle(mapping).map_err(|e| format!("mapping.ttl: {e}"))?;
    if let Some(base) = base_iri {
        let base = BaseIri::new(base).map_err(|e| format!("case.toml: {e}"))?;
        mapping = mapping.with_base_iri(base);
    }
    Ok(Planner::new(mapping))
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
    match planner(mapping, base_iri)?.plan(query) {
        Err(v12n_core::Error::UnsupportedFeature(feature)) => Ok(Some(feature)),
        Err(e) => Err(format!("expected an unsupported-feature error, got: {e}")),
        Ok(_) => Ok(None),
    }
}

/// The error that loading an invalid mapping gives, which `v12n check`
/// reports with exit code 2. An `UnsupportedFeature` error does not count:
/// the mapping may be valid, and the check would pass for the wrong reason.
pub fn mapping_error(mapping: &str) -> Result<String, String> {
    match Mapping::from_turtle(mapping) {
        Err(v12n_core::Error::UnsupportedFeature(feature)) => Err(format!(
            "expected an invalid mapping, but it is rejected as unsupported: {feature}"
        )),
        Err(e) => Ok(e.to_string()),
        Ok(_) => Err("expected a mapping error, but the mapping loads".to_owned()),
    }
}

/// All solutions, with no row limit so that a cut-off result cannot pass
/// for a complete one.
///
/// The query runs twice on one connection and the second result is the
/// one compared: the second run takes the statements prepared by the first
/// as its guess (`PostgresBackend` remembers them per connection), which a
/// single run never does. Both runs must give the same bag of solutions.
pub fn run(
    database_url: &str,
    mapping: &str,
    base_iri: Option<&str>,
    query: &str,
) -> Result<Results, Failure> {
    let planner = Arc::new(planner(mapping, base_iri).map_err(Failure::Other)?);
    let plan = planner.plan(query).map_err(|e| match e {
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
            let backend = PostgresConnector::new(database_url).connect(limits).await?;
            let engine = Engine::new(planner, backend);
            let first = solutions(&engine, &plan).await?;
            let second = solutions(&engine, &plan).await?;
            Ok::<_, v12n_postgres::Error>((first, second))
        })
        .map_err(|e| match e.kind() {
            ErrorKind::Core(v12n_core::Error::UnsupportedFeature(feature)) => {
                Failure::Unsupported(feature.clone())
            }
            _ => Failure::Other(e.to_string()),
        })
        .and_then(|(first, second)| {
            if first.variables == second.variables && bag(&first.rows) == bag(&second.rows) {
                Ok(second)
            } else {
                Err(Failure::Other(format!(
                    "a second run on the same connection differs from the first:\n  \
                     first: {first:?}\n  second: {second:?}"
                )))
            }
        })
}

async fn solutions(
    engine: &Engine<PostgresBackend>,
    plan: &SqlPlan,
) -> Result<Results, v12n_postgres::Error> {
    let mut solutions = engine.execute(plan).await?;
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
    Ok(Results { variables, rows })
}

fn bag(rows: &[Row]) -> HashMap<&Row, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        *counts.entry(row).or_default() += 1;
    }
    counts
}

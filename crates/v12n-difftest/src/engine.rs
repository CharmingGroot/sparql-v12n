// SPDX-License-Identifier: Apache-2.0
//! Runs a query through this engine with library calls, as `v12n query`
//! does, without the CLI or HTTP layers.

use v12n_core::mapping::{Mapping, MappingIndex};
use v12n_core::sql::SqlPlan;
use v12n_core::{sparql, unfold};
use v12n_postgres::{Database, Limits};

use crate::compare::{Results, Row};

/// Unfolds the query; the outer error is a broken mapping, which is a
/// problem with the case rather than with the query.
fn unfold_query(mapping: &str, query: &str) -> Result<Result<SqlPlan, v12n_core::Error>, String> {
    let mapping = Mapping::from_turtle(mapping).map_err(|e| format!("mapping.ttl: {e}"))?;
    Ok(sparql::parse(query)
        .and_then(|query| unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping))))
}

/// The unsupported feature the engine names when it rejects the query,
/// or why the query was not rejected that way. The CLI turns this error
/// into exit code 1 and the HTTP endpoint into status 501.
pub fn check_rejected(mapping: &str, query: &str) -> Result<String, String> {
    match unfold_query(mapping, query)? {
        Err(v12n_core::Error::UnsupportedFeature(feature)) => Ok(feature),
        Err(e) => Err(format!("expected an unsupported-feature error, got: {e}")),
        Ok(_) => Err(
            "expected an unsupported-feature error, but the engine accepts the query; \
             remove `expect = \"rejected\"` from expect.toml so it is compared with Ontop"
                .to_owned(),
        ),
    }
}

/// All solutions, with no row limit so that a cut-off result cannot pass
/// for a complete one.
pub fn run(database_url: &str, mapping: &str, query: &str) -> Result<Results, String> {
    let plan = unfold_query(mapping, query)?.map_err(|e| match e {
        v12n_core::Error::UnsupportedFeature(_) => {
            format!(
                "{e}; if this is intended, mark the query `expect = \"rejected\"` in expect.toml"
            )
        }
        e => e.to_string(),
    })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
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
        .map_err(|e| e.to_string())
}

// SPDX-License-Identifier: Apache-2.0
//! Reading the mapping and the query, from files or stdin.

use std::io::{self, Read};
use std::path::Path;

use serde_json::json;
use v12n_core::Planner;
use v12n_core::mapping::{BaseIri, Mapping};
use v12n_core::sql::SqlPlan;

use crate::args::QueryArg;
use crate::failure::{EXIT_INPUT, EXIT_MAPPING, Failure};

pub(crate) fn read_input(path: &Path) -> Result<String, Failure> {
    let mut text = String::new();
    let read = if path == Path::new("-") {
        io::stdin().read_to_string(&mut text).map(|_| ())
    } else {
        std::fs::read_to_string(path).map(|t| text = t)
    };
    read.map_err(|e| Failure {
        details: Some(json!({ "path": path.display().to_string() })),
        ..Failure::new(
            EXIT_INPUT,
            "input",
            format!("cannot read {}: {e}", path.display()),
        )
    })?;
    Ok(text)
}

pub(crate) fn load_mapping(path: &Path) -> Result<Mapping, Failure> {
    let text = read_input(path)?;
    Mapping::from_turtle(&text).map_err(|e| Failure::new(EXIT_MAPPING, "mapping", e.to_string()))
}

/// Reads the mapping and the query and unfolds the query.
pub(crate) fn load_plan(
    mapping_path: &Path,
    query: &QueryArg,
    base_iri: Option<&BaseIri>,
) -> Result<(Planner, SqlPlan), Failure> {
    let stdin = Path::new("-");
    if mapping_path == stdin && query.file.as_deref() == Some(stdin) {
        return Err(Failure::new(
            EXIT_INPUT,
            "input",
            "only one of the mapping and the query can be read from stdin",
        ));
    }
    let mut mapping = load_mapping(mapping_path)?;
    if let Some(base) = base_iri {
        mapping = mapping.with_base_iri(base.clone());
    }
    let text = match (&query.file, &query.text) {
        (_, Some(text)) => text.clone(),
        (Some(path), None) => read_input(path)?,
        // clap requires one of them.
        (None, None) => unreachable!("clap requires a query"),
    };
    let planner = Planner::new(mapping);
    // Errors past this point are about the query, not the mapping.
    let plan = planner.plan(&text).map_err(|e| {
        let kind = match e {
            v12n_core::Error::QuerySyntax(_) => "query",
            _ => "unsupported",
        };
        Failure::new(EXIT_INPUT, kind, e.to_string())
    })?;
    Ok((planner, plan))
}

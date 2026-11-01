// SPDX-License-Identifier: Apache-2.0
//! `v12n check`: validate a mapping, and with `--db` its tables and columns.

use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use v12n_core::backend::Connector;
use v12n_core::{Engine, Limits, Planner};

use crate::args::Format;
use crate::failure::{EXIT_DATABASE, EXIT_MAPPING, Failure, database_failure};
use crate::input::load_mapping;
use crate::output::{Output, print_csv};

pub(crate) fn check(
    path: &Path,
    connector: Option<impl Connector>,
    output: Output,
) -> Result<(), Failure> {
    let planner = Arc::new(Planner::new(load_mapping(path)?));
    if let Some(connector) = connector {
        check_catalog(&planner, connector)?;
    }
    let index = planner.index();
    let triples_maps = planner.mapping().triples_maps().len();
    let classes = index.classes().count();
    let properties = index.properties().count();
    match output.format {
        Format::Json => println!(
            "{}",
            json!({
                "ok": true,
                "triples_maps": triples_maps,
                "classes": classes,
                "properties": properties,
            })
        ),
        Format::Csv => print_csv(&[
            vec![
                "triples_maps".to_owned(),
                "classes".into(),
                "properties".into(),
            ],
            vec![
                triples_maps.to_string(),
                classes.to_string(),
                properties.to_string(),
            ],
        ])?,
        Format::Table => {
            println!("ok: {triples_maps} triples maps, {classes} classes, {properties} properties")
        }
        Format::Xml | Format::Tsv => unreachable!("refused in main"),
    }
    Ok(())
}

/// Fails, as an invalid mapping, when the database lacks a table or column
/// that the mapping reads or refuses to read one.
fn check_catalog(planner: &Arc<Planner>, connector: impl Connector) -> Result<(), Failure> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::new(EXIT_DATABASE, "database", e.to_string()))?;
    let snapshot = runtime
        .block_on(async {
            let backend = connector.connect(Limits::default()).await?;
            Engine::new(Arc::clone(planner), Arc::default(), backend)
                .load_catalog()
                .await
        })
        .map_err(|e| database_failure(&e))?;
    let problems: Vec<String> = snapshot
        .problems(planner.catalog_request())
        .iter()
        .map(ToString::to_string)
        .collect();
    if problems.is_empty() {
        return Ok(());
    }
    Err(Failure {
        details: Some(json!({ "problems": problems })),
        ..Failure::new(
            EXIT_MAPPING,
            "catalog",
            format!(
                "the database lacks what the mapping reads: {}",
                problems.join("; ")
            ),
        )
    })
}

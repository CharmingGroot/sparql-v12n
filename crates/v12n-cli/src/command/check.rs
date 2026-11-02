// SPDX-License-Identifier: Apache-2.0
//! `v12n check`: validate a mapping, and with `--db` its tables and columns.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use v12n_core::backend::Connector;
use v12n_core::mapping::{Mapping, TermMapValue, TriplesMap};
use v12n_core::unfold::shared_subject_templates;
use v12n_core::{Engine, Limits, Planner};

use crate::args::Format;
use crate::failure::{EXIT_DATABASE, EXIT_MAPPING, Failure, database_failure};
use crate::input::load_mapping;
use crate::output::{Output, YELLOW, paint, print_csv, stream_color};

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
    let warnings = warnings(planner.mapping());
    let color = stream_color(io::stderr().is_terminal());
    for warning in &warnings {
        eprintln!(
            "{} {}",
            paint(color, YELLOW, "warning:"),
            warning["message"].as_str().unwrap_or_default()
        );
    }
    match output.format {
        Format::Json => println!(
            "{}",
            json!({
                "ok": true,
                "triples_maps": triples_maps,
                "classes": classes,
                "properties": properties,
                "warnings": warnings,
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

/// Valid mappings whose queries may return more than their author expects.
/// A warning leaves the exit code at 0.
fn warnings(mapping: &Mapping) -> Vec<Value> {
    shared_subject_templates(mapping)
        .into_iter()
        .map(|(left, right)| {
            json!({
                "kind": "shared_subject",
                "triples_maps": [left.id.to_string(), right.id.to_string()],
                "subject_templates": [subject_template(left), subject_template(right)],
                "message": format!(
                    "{} and {} read different logical tables, and their subject templates \
                     {} and {} can produce the same IRI; rows with the same template values \
                     describe one subject, and a basic graph pattern returns every \
                     combination of that subject's values from both tables",
                    left.id,
                    right.id,
                    subject_template(left),
                    subject_template(right),
                ),
            })
        })
        .collect()
}

fn subject_template(triples_map: &TriplesMap) -> &str {
    match &triples_map.subject_map.term_map.value {
        TermMapValue::Template(template) => template.as_str(),
        TermMapValue::Constant(_) | TermMapValue::Column(_) => "",
    }
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

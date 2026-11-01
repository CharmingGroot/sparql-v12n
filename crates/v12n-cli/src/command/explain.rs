// SPDX-License-Identifier: Apache-2.0
//! `v12n explain`: the SQL of a query, without a database.

use std::path::Path;

use serde_json::{Value, json};
use v12n_core::mapping::{BaseIri, TermType};

use crate::args::{Format, QueryArg};
use crate::failure::{EXIT_INPUT, Failure};
use crate::input::load_plan;
use crate::output::{Output, print_csv};

pub(crate) fn explain(
    mapping_path: &Path,
    query: &QueryArg,
    base_iri: Option<&BaseIri>,
    output: Output,
) -> Result<(), Failure> {
    let (_, plan) = load_plan(mapping_path, query, base_iri)?;
    // Rendering fails only for a plan that SQL cannot answer as SPARQL does.
    let rendered = plan
        .to_sql()
        .map_err(|e| Failure::new(EXIT_INPUT, "unsupported", e.to_string()))?;

    match output.format {
        Format::Json => {
            let variables: Vec<Value> = plan
                .projection
                .iter()
                .map(|p| {
                    json!({
                        "name": p.variable,
                        "term_type": p.terms.first().map(|t| term_type(t.term_type)),
                        "columns": p.terms.iter().flat_map(|t| t.value.columns()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            println!(
                "{}",
                json!({ "sql": rendered.sql, "params": rendered.params, "variables": variables })
            );
        }
        Format::Csv => {
            let header: Vec<String> = std::iter::once("sql".to_owned())
                .chain((1..=rendered.params.len()).map(|i| format!("${i}")))
                .collect();
            let record: Vec<&str> = std::iter::once(rendered.sql.as_str())
                .chain(rendered.params.iter().map(String::as_str))
                .collect();
            let header: Vec<&str> = header.iter().map(String::as_str).collect();
            print_csv(&[header, record])?;
        }
        Format::Table => {
            println!("{}", rendered.sql);
            if !rendered.params.is_empty() {
                println!();
                for (i, param) in rendered.params.iter().enumerate() {
                    println!("${} = {}", i + 1, Value::from(param.as_str()));
                }
            }
        }
        Format::Xml | Format::Tsv => unreachable!("refused in main"),
    }
    Ok(())
}

fn term_type(term_type: TermType) -> &'static str {
    match term_type {
        TermType::Iri => "iri",
        TermType::BlankNode => "blank_node",
        TermType::Literal => "literal",
    }
}

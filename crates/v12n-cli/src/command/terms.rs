// SPDX-License-Identifier: Apache-2.0
//! `v12n terms`: the classes and properties a mapping exposes.

use std::path::Path;

use serde_json::{Value, json};
use v12n_core::Planner;
use v12n_core::mapping::TriplesMapId;

use crate::args::Format;
use crate::failure::Failure;
use crate::input::load_mapping;
use crate::output::{Output, print_csv, print_table};

pub(crate) fn terms(path: &Path, output: Output) -> Result<(), Failure> {
    let planner = Planner::new(load_mapping(path)?);
    let (mapping, index) = (planner.mapping(), planner.index());
    let classes: Vec<(&str, Vec<&TriplesMapId>)> = index
        .classes()
        .map(|(iri, ids)| (iri, dedup(ids.iter())))
        .collect();
    let properties: Vec<(&str, Vec<&TriplesMapId>)> = index
        .properties()
        .map(|(iri, candidates)| (iri, dedup(candidates.iter().map(|c| &c.triples_map))))
        .collect();
    // Only the prefixes the listed terms use, so they can be pasted into a
    // query as they are.
    let mut used: Vec<(&str, &str)> = Vec::new();
    for (iri, _) in classes.iter().chain(&properties) {
        if let Some(name) = mapping.compact_iri(iri) {
            let prefix = &name[..name.find(':').unwrap_or_default()];
            if let Some((prefix, namespace)) = mapping.prefixes().get_key_value(prefix)
                && !used.contains(&(prefix.as_str(), namespace.as_str()))
            {
                used.push((prefix, namespace));
            }
        }
    }
    used.sort_unstable();
    let kinds = || {
        classes
            .iter()
            .map(|t| ("class", t))
            .chain(properties.iter().map(|t| ("property", t)))
    };
    let ids = |ids: &[&TriplesMapId]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();

    match output.format {
        Format::Json => {
            // `iri` stays absolute, so a reader needs no prefix handling.
            let entries = |terms: &[(&str, Vec<&TriplesMapId>)]| -> Vec<Value> {
                terms
                    .iter()
                    .map(|(iri, triples_maps)| {
                        json!({
                            "iri": iri,
                            "prefixed_name": mapping.compact_iri(iri),
                            "triples_maps": ids(triples_maps),
                        })
                    })
                    .collect()
            };
            let prefixes: serde_json::Map<String, Value> = used
                .iter()
                .map(|(prefix, namespace)| ((*prefix).to_owned(), Value::from(*namespace)))
                .collect();
            println!(
                "{}",
                json!({
                    "prefixes": prefixes,
                    "classes": entries(&classes),
                    "properties": entries(&properties),
                })
            );
        }
        Format::Csv => {
            let header = ["kind", "iri", "prefixed_name", "triples_maps"].map(str::to_owned);
            let records: Vec<Vec<String>> = std::iter::once(header.to_vec())
                .chain(kinds().map(|(kind, (iri, triples_maps))| {
                    vec![
                        kind.to_owned(),
                        (*iri).to_owned(),
                        mapping.compact_iri(iri).unwrap_or_default(),
                        ids(triples_maps).join(" "),
                    ]
                }))
                .collect();
            print_csv(&records)?;
        }
        Format::Table => {
            for (prefix, namespace) in &used {
                println!("PREFIX {prefix}: <{namespace}>");
            }
            if !used.is_empty() {
                println!();
            }
            let rows: Vec<[String; 3]> = kinds()
                .map(|(kind, (iri, triples_maps))| {
                    let term = mapping
                        .compact_iri(iri)
                        .unwrap_or_else(|| format!("<{iri}>"));
                    [kind.to_owned(), term, ids(triples_maps).join(", ")]
                })
                .collect();
            print_table(["KIND", "TERM", "TRIPLES MAPS"], &rows, output.color);
        }
        Format::Xml | Format::Tsv => unreachable!("refused in main"),
    }
    Ok(())
}

/// A triples map can provide the same property through several
/// predicate-object maps; `terms` lists it once.
fn dedup<'a>(ids: impl Iterator<Item = &'a TriplesMapId>) -> Vec<&'a TriplesMapId> {
    let mut out: Vec<&TriplesMapId> = Vec::new();
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

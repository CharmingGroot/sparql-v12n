// SPDX-License-Identifier: Apache-2.0
//! `v12n`: check mappings, list terms, explain and run SPARQL, serve HTTP.

use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use v12n_core::mapping::{Mapping, MappingIndex, TermType, TriplesMapId};
use v12n_core::{sparql, unfold};

/// Exit codes are part of the CLI contract; scripts and agents rely on them.
const EXIT_INPUT: u8 = 1;
const EXIT_MAPPING: u8 = 2;

#[derive(Parser)]
#[command(
    name = "v12n",
    version,
    about = "Query PostgreSQL with SPARQL through R2RML mappings"
)]
struct Cli {
    /// Output format [default: table on a terminal, json otherwise]
    #[arg(long, global = true, value_enum)]
    format: Option<Format>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Json,
    Table,
}

#[derive(Subcommand)]
enum Command {
    /// Check a mapping for syntax and reference errors
    #[command(after_help = "Examples:\n  v12n check mapping.ttl\n  cat mapping.ttl | v12n check -")]
    Check {
        /// R2RML mapping in Turtle, or `-` for stdin
        mapping: PathBuf,
    },
    /// List the classes and properties a mapping exposes
    #[command(
        after_help = "Examples:\n  v12n terms mapping.ttl\n  v12n terms mapping.ttl --format json"
    )]
    Terms {
        /// R2RML mapping in Turtle, or `-` for stdin
        mapping: PathBuf,
    },
    /// Print the SQL a SPARQL query unfolds to, without a database
    #[command(after_help = "Examples:\n  v12n explain mapping.ttl query.rq\n  \
                            echo 'SELECT ?c WHERE { ?c a <http://example.org/Customer> }' | v12n explain mapping.ttl -\n  \
                            v12n explain mapping.ttl query.rq --format json")]
    Explain {
        /// R2RML mapping in Turtle, or `-` for stdin
        mapping: PathBuf,
        /// SPARQL SELECT query, or `-` for stdin
        query: PathBuf,
    },
    /// Run a SPARQL query against the database
    Query { mapping: PathBuf, query: PathBuf },
    /// Serve the SPARQL 1.1 Protocol endpoint
    Serve,
}

/// A failure to report: exit code, error kind for JSON output, message.
struct Failure {
    code: u8,
    kind: &'static str,
    message: String,
}

fn main() -> ExitCode {
    // clap exits with 2 on usage errors, which would read as a mapping error.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { EXIT_INPUT } else { 0 });
        }
    };
    let format = cli.format.unwrap_or(if io::stdout().is_terminal() {
        Format::Table
    } else {
        Format::Json
    });
    let result = match &cli.command {
        Command::Check { mapping } => check(mapping, format),
        Command::Terms { mapping } => terms(mapping, format),
        Command::Explain { mapping, query } => explain(mapping, query, format),
        Command::Query { .. } => not_implemented("query"),
        Command::Serve => not_implemented("serve"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            match format {
                Format::Json => println!(
                    "{}",
                    json!({
                        "ok": false,
                        "error": { "kind": failure.kind, "message": failure.message },
                    })
                ),
                Format::Table => eprintln!("error: {}", failure.message),
            }
            ExitCode::from(failure.code)
        }
    }
}

fn not_implemented(command: &str) -> Result<(), Failure> {
    Err(Failure {
        code: EXIT_INPUT,
        kind: "not_implemented",
        message: format!("v12n {command}: not implemented yet"),
    })
}

fn read_input(path: &Path) -> Result<String, Failure> {
    let mut text = String::new();
    let read = if path == Path::new("-") {
        io::stdin().read_to_string(&mut text).map(|_| ())
    } else {
        std::fs::read_to_string(path).map(|t| text = t)
    };
    read.map_err(|e| Failure {
        code: EXIT_INPUT,
        kind: "input",
        message: format!("cannot read {}: {e}", path.display()),
    })?;
    Ok(text)
}

fn load_mapping(path: &Path) -> Result<Mapping, Failure> {
    let text = read_input(path)?;
    Mapping::from_turtle(&text).map_err(|e| Failure {
        code: EXIT_MAPPING,
        kind: "mapping",
        message: e.to_string(),
    })
}

fn check(path: &Path, format: Format) -> Result<(), Failure> {
    let mapping = load_mapping(path)?;
    let index = MappingIndex::new(&mapping);
    let triples_maps = mapping.triples_maps().len();
    let classes = index.classes().count();
    let properties = index.properties().count();
    match format {
        Format::Json => println!(
            "{}",
            json!({
                "ok": true,
                "triples_maps": triples_maps,
                "classes": classes,
                "properties": properties,
            })
        ),
        Format::Table => {
            println!("ok: {triples_maps} triples maps, {classes} classes, {properties} properties")
        }
    }
    Ok(())
}

fn terms(path: &Path, format: Format) -> Result<(), Failure> {
    let mapping = load_mapping(path)?;
    let index = MappingIndex::new(&mapping);
    let classes: Vec<(&str, Vec<&TriplesMapId>)> = index
        .classes()
        .map(|(iri, ids)| (iri, dedup(ids.iter())))
        .collect();
    let properties: Vec<(&str, Vec<&TriplesMapId>)> = index
        .properties()
        .map(|(iri, candidates)| (iri, dedup(candidates.iter().map(|c| &c.triples_map))))
        .collect();

    match format {
        Format::Json => {
            let entries = |terms: &[(&str, Vec<&TriplesMapId>)]| -> Vec<Value> {
                terms
                    .iter()
                    .map(|(iri, ids)| {
                        json!({
                            "iri": iri,
                            "triples_maps": ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
                        })
                    })
                    .collect()
            };
            println!(
                "{}",
                json!({ "classes": entries(&classes), "properties": entries(&properties) })
            );
        }
        Format::Table => {
            let rows: Vec<[String; 3]> = classes
                .iter()
                .map(|t| ("class", t))
                .chain(properties.iter().map(|t| ("property", t)))
                .map(|(kind, (iri, ids))| {
                    let ids: Vec<_> = ids.iter().map(|id| id.to_string()).collect();
                    [kind.to_owned(), format!("<{iri}>"), ids.join(", ")]
                })
                .collect();
            print_table(["KIND", "TERM", "TRIPLES MAPS"], &rows);
        }
    }
    Ok(())
}

fn explain(mapping_path: &Path, query_path: &Path, format: Format) -> Result<(), Failure> {
    let stdin = Path::new("-");
    if mapping_path == stdin && query_path == stdin {
        return Err(Failure {
            code: EXIT_INPUT,
            kind: "input",
            message: "only one of the mapping and the query can be read from stdin".to_owned(),
        });
    }
    let mapping = load_mapping(mapping_path)?;
    let query = read_input(query_path)?;
    // Errors past this point are about the query, not the mapping.
    let plan = sparql::parse(&query)
        .and_then(|query| unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)))
        .map_err(|e| Failure {
            code: EXIT_INPUT,
            kind: match e {
                v12n_core::Error::QuerySyntax(_) => "query",
                _ => "unsupported",
            },
            message: e.to_string(),
        })?;
    let rendered = plan.to_sql();

    match format {
        Format::Json => {
            let variables: Vec<Value> = plan
                .projection
                .iter()
                .map(|p| {
                    json!({
                        "name": p.variable,
                        "term_type": p.term.as_ref().map(|t| term_type(t.term_type)),
                        "columns": p.term.as_ref().map(|t| t.value.columns()).unwrap_or_default(),
                    })
                })
                .collect();
            println!(
                "{}",
                json!({ "sql": rendered.sql, "params": rendered.params, "variables": variables })
            );
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

fn print_table<const N: usize>(header: [&str; N], rows: &[[String; N]]) {
    let mut widths = header.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: [&str; N]| {
        let padded: Vec<_> = cells
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(header);
    for row in rows {
        line(row.each_ref().map(String::as_str));
    }
}

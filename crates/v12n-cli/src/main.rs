// SPDX-License-Identifier: Apache-2.0
//! `v12n`: check mappings, list terms, explain and run SPARQL, serve HTTP.

use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use v12n_core::mapping::{Mapping, MappingIndex, TermType, TriplesMapId};
use v12n_core::results::JsonResultsWriter;
use v12n_core::sql::SqlPlan;
use v12n_core::{sparql, unfold};
use v12n_postgres::{DEFAULT_ROW_LIMIT, Database, Limits, Solutions};

/// Exit codes are part of the CLI contract; scripts and agents rely on them.
const EXIT_INPUT: u8 = 1;
const EXIT_MAPPING: u8 = 2;
const EXIT_DATABASE: u8 = 3;
const EXIT_LIMIT: u8 = 4;

/// Shared by the top-level and `query` help, which `concat!` needs as a literal.
macro_rules! exit_codes_help {
    () => {
        "Exit codes:
  0  success
  1  invalid input: arguments, files, query syntax, unsupported SPARQL
  2  invalid mapping
  3  database error, or a value that cannot become an RDF term
  4  limit reached: statement timeout, or more rows than --row-limit"
    };
}

const QUERY_HELP: &str = concat!(
    "Examples:
  v12n query mapping.ttl query.rq --db postgresql://reader@localhost/shop
  V12N_DATABASE_URL=postgresql://reader@localhost/shop v12n query mapping.ttl query.rq
  echo 'SELECT ?c WHERE { ?c a <http://example.org/Customer> }' | v12n query mapping.ttl - --format table

JSON output is SPARQL 1.1 Query Results JSON, written as rows arrive.
The session is read-only; still, connect as a user that can only read.

When more rows exist than --row-limit, the first --row-limit solutions are
printed as a complete result and the command exits with 4. An error after
output has started is reported on stderr and leaves stdout incomplete.

",
    exit_codes_help!()
);

#[derive(Parser)]
#[command(
    name = "v12n",
    version,
    about = "Query PostgreSQL with SPARQL through R2RML mappings",
    after_help = exit_codes_help!()
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
    /// Run a SPARQL query on PostgreSQL and print the solutions
    #[command(after_help = QUERY_HELP)]
    Query {
        /// R2RML mapping in Turtle, or `-` for stdin
        mapping: PathBuf,
        /// SPARQL SELECT query, or `-` for stdin
        query: PathBuf,
        /// PostgreSQL connection URL, e.g. postgresql://reader@localhost/shop
        #[arg(long, env = "V12N_DATABASE_URL", hide_env_values = true)]
        db: Option<String>,
        /// Most solutions to return; 0 means no limit
        #[arg(long, default_value_t = DEFAULT_ROW_LIMIT)]
        row_limit: u64,
        /// Statement timeout such as 500ms, 30s or 2m; 0 disables it
        #[arg(long, default_value = "30s", value_parser = parse_duration)]
        timeout: Duration,
    },
    /// Serve the SPARQL 1.1 Protocol endpoint
    Serve,
}

/// A failure to report: exit code, error kind for JSON output, message.
struct Failure {
    code: u8,
    kind: &'static str,
    message: String,
    /// Results were already written to stdout, so the error goes to stderr
    /// even in JSON format.
    after_output: bool,
}

impl Failure {
    fn new(code: u8, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind,
            message: message.into(),
            after_output: false,
        }
    }
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
        Command::Query {
            mapping,
            query: query_path,
            db,
            row_limit,
            timeout,
        } => query(
            mapping,
            query_path,
            db.as_deref(),
            Limits {
                statement_timeout: *timeout,
                row_limit: *row_limit,
            },
            format,
        ),
        Command::Serve => not_implemented("serve"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            match format {
                _ if failure.after_output => eprintln!("error: {}", failure.message),
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
    Err(Failure::new(
        EXIT_INPUT,
        "not_implemented",
        format!("v12n {command}: not implemented yet"),
    ))
}

/// `500ms`, `30s`, `2m`, or a bare number of seconds.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "s"),
    };
    let number: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} is not a duration such as 500ms, 30s or 2m"))?;
    match unit {
        "ms" => Ok(Duration::from_millis(number)),
        "s" => Ok(Duration::from_secs(number)),
        "m" => Ok(Duration::from_secs(number.saturating_mul(60))),
        _ => Err(format!("unknown unit {unit:?}; use ms, s or m")),
    }
}

fn read_input(path: &Path) -> Result<String, Failure> {
    let mut text = String::new();
    let read = if path == Path::new("-") {
        io::stdin().read_to_string(&mut text).map(|_| ())
    } else {
        std::fs::read_to_string(path).map(|t| text = t)
    };
    read.map_err(|e| {
        Failure::new(
            EXIT_INPUT,
            "input",
            format!("cannot read {}: {e}", path.display()),
        )
    })?;
    Ok(text)
}

fn load_mapping(path: &Path) -> Result<Mapping, Failure> {
    let text = read_input(path)?;
    Mapping::from_turtle(&text).map_err(|e| Failure::new(EXIT_MAPPING, "mapping", e.to_string()))
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

/// Reads the mapping and the query and unfolds the query.
fn load_plan(mapping_path: &Path, query_path: &Path) -> Result<SqlPlan, Failure> {
    let stdin = Path::new("-");
    if mapping_path == stdin && query_path == stdin {
        return Err(Failure::new(
            EXIT_INPUT,
            "input",
            "only one of the mapping and the query can be read from stdin",
        ));
    }
    let mapping = load_mapping(mapping_path)?;
    let query = read_input(query_path)?;
    // Errors past this point are about the query, not the mapping.
    sparql::parse(&query)
        .and_then(|query| unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)))
        .map_err(|e| {
            let kind = match e {
                v12n_core::Error::QuerySyntax(_) => "query",
                _ => "unsupported",
            };
            Failure::new(EXIT_INPUT, kind, e.to_string())
        })
}

fn explain(mapping_path: &Path, query_path: &Path, format: Format) -> Result<(), Failure> {
    let plan = load_plan(mapping_path, query_path)?;
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

fn query(
    mapping_path: &Path,
    query_path: &Path,
    db: Option<&str>,
    limits: Limits,
    format: Format,
) -> Result<(), Failure> {
    let plan = load_plan(mapping_path, query_path)?;
    let url = db.ok_or_else(|| {
        Failure::new(
            EXIT_INPUT,
            "input",
            "no database: pass --db or set V12N_DATABASE_URL",
        )
    })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::new(EXIT_DATABASE, "database", e.to_string()))?;
    runtime.block_on(async {
        let database = Database::connect(url, limits)
            .await
            .map_err(database_failure)?;
        let mut solutions = database.execute(&plan).await.map_err(database_failure)?;
        // Waiting for the first row before printing anything means an error
        // in planning or early execution is still reported in `format`.
        let first = solutions.try_next().await.map_err(database_failure)?;
        let printed = match format {
            Format::Json => print_json(&mut solutions, first).await?,
            Format::Table => print_solutions_table(&mut solutions, first).await?,
        };
        if solutions.is_truncated() {
            return Err(Failure {
                after_output: true,
                ..Failure::new(
                    EXIT_LIMIT,
                    "row_limit",
                    format!("stopped after {printed} rows (--row-limit); more rows exist"),
                )
            });
        }
        Ok(())
    })
}

fn database_failure(error: v12n_postgres::Error) -> Failure {
    use v12n_postgres::Error;
    let (code, kind) = match &error {
        Error::Config(_) => (EXIT_INPUT, "input"),
        Error::Database(_) => (EXIT_DATABASE, "database"),
        Error::Timeout(_) => (EXIT_LIMIT, "timeout"),
        Error::Core(_) => (EXIT_DATABASE, "data"),
    };
    Failure::new(code, kind, error.to_string())
}

/// Streams the solutions as SPARQL 1.1 Query Results JSON and returns how
/// many were written.
async fn print_json(
    solutions: &mut Solutions,
    first: Option<v12n_core::solution::Solution>,
) -> Result<u64, Failure> {
    let output_failure = |e: io::Error| Failure {
        after_output: true,
        ..Failure::new(EXIT_INPUT, "output", format!("cannot write output: {e}"))
    };
    let stdout = BufWriter::new(io::stdout().lock());
    let mut writer =
        JsonResultsWriter::start(stdout, solutions.variables()).map_err(output_failure)?;
    let mut count = 0;
    let mut next = first;
    while let Some(solution) = next {
        match writer.write(&solution) {
            Ok(()) => count += 1,
            // The reader went away, e.g. `| head`; nothing left to report to.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(count),
            Err(e) => return Err(output_failure(e)),
        }
        next = match solutions.try_next().await {
            Ok(next) => next,
            Err(e) => {
                // Partial JSON stays unterminated so it cannot pass for a
                // complete result.
                let _ = io::stdout().flush();
                return Err(Failure {
                    after_output: true,
                    ..database_failure(e)
                });
            }
        };
    }
    match writer.finish().and_then(|mut out| out.flush()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(output_failure(e)),
        _ => Ok(count),
    }
}

/// Prints the solutions as an aligned table, terms in N-Triples syntax and
/// unbound variables as empty cells. Alignment needs every row first; the
/// row limit bounds how many that is.
async fn print_solutions_table(
    solutions: &mut Solutions,
    first: Option<v12n_core::solution::Solution>,
) -> Result<u64, Failure> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut next = first;
    while let Some(solution) = next {
        rows.push(
            solution
                .values()
                .iter()
                .map(|v| v.as_ref().map(ToString::to_string).unwrap_or_default())
                .collect(),
        );
        next = solutions.try_next().await.map_err(database_failure)?;
    }
    let header: Vec<String> = solutions
        .variables()
        .iter()
        .map(|v| format!("?{v}"))
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = BufWriter::new(io::stdout().lock());
    let mut result = Ok(());
    for row in std::iter::once(&header).chain(&rows) {
        let padded: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, &width)| format!("{cell:<width$}"))
            .collect();
        result = writeln!(out, "{}", padded.join("  ").trim_end());
        if result.is_err() {
            break;
        }
    }
    match result.and_then(|()| out.flush()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(Failure::new(
            EXIT_INPUT,
            "output",
            format!("cannot write output: {e}"),
        )),
        _ => Ok(rows.len() as u64),
    }
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

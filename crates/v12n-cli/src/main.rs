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
use v12n_server::config::{Config, env, parse_duration};

/// Exit codes are part of the CLI contract; scripts and agents rely on them.
const EXIT_INPUT: u8 = 1;
const EXIT_MAPPING: u8 = 2;
const EXIT_DATABASE: u8 = 3;
const EXIT_TIMEOUT: u8 = 4;
/// Separate from [`EXIT_TIMEOUT`] because the fix differs: a higher limit
/// or paging with LIMIT/OFFSET, not a faster query or a longer timeout.
const EXIT_ROW_LIMIT: u8 = 5;

/// Shared by the top-level and `query` help, which `concat!` needs as a literal.
macro_rules! exit_codes_help {
    () => {
        "Exit codes:
  0  success
  1  invalid input: arguments, files, query syntax, unsupported SPARQL
  2  invalid mapping
  3  database error, or a value that cannot become an RDF term
  4  statement timeout (--timeout) reached
  5  row limit reached: more rows exist than --row-limit"
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
printed as a complete result, a warning naming the limit goes to stderr,
and the command exits with 5. A result of exactly --row-limit rows is not
truncated and exits with 0.

When the statement timeout cancels the query, stderr names the timeout and
the command exits with 4. Before the first row, the error is reported in
--format like other errors; after output has started, stdout is left
incomplete so that it cannot pass for a complete result.

",
    exit_codes_help!()
);

const SERVE_HELP: &str = "Examples:
  v12n serve --config v12n.toml
  V12N_DATABASE_URL=postgresql://reader@localhost/shop V12N_MAPPING=mapping.ttl v12n serve
  curl -H 'Accept: application/sparql-results+json' --data-urlencode query@query.rq http://127.0.0.1:7878/sparql

Settings (file key, environment variable, default):
  [server] address                  V12N_ADDRESS                  127.0.0.1
  [server] port                     V12N_PORT                     7878
  [database] url                    V12N_DATABASE_URL             required
  [database] password_env           (file only)                   none
  [mapping] path                    V12N_MAPPING                  required
  [limits] statement_timeout        V12N_STATEMENT_TIMEOUT        30s (0: none)
  [limits] row_limit                V12N_ROW_LIMIT                10000 (0: none)
  [limits] max_query_bytes          V12N_MAX_QUERY_BYTES          65536
  [limits] max_concurrent_requests  V12N_MAX_CONCURRENT_REQUESTS  16

Environment variables override the file. Other V12N_* variables are
ignored with a warning in the log.

A relative mapping path in the file is relative to the file. Connect as a
database user that can only read; sessions are also set read-only.
Logs go to stderr; RUST_LOG sets the level (default info).

Exit codes: 1 invalid settings or address in use, 2 invalid mapping,
3 database unreachable at startup.";

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
        #[arg(long, env = env::DATABASE_URL, hide_env_values = true)]
        db: Option<String>,
        /// Most solutions to return; 0 means no limit
        #[arg(long, env = env::ROW_LIMIT, default_value_t = DEFAULT_ROW_LIMIT)]
        row_limit: u64,
        /// Statement timeout such as 500ms, 30s or 2m; 0 disables it
        #[arg(long, env = env::STATEMENT_TIMEOUT, default_value = "30s", value_parser = parse_duration)]
        timeout: Duration,
    },
    /// Serve the SPARQL 1.1 Protocol endpoint at /sparql
    #[command(after_help = SERVE_HELP)]
    Serve {
        /// TOML settings file; the environment variables below override it
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// A failure to report: exit code, error kind for JSON output, message.
struct Failure {
    code: u8,
    kind: &'static str,
    message: String,
    /// Results were already written to stdout, so the error goes to stderr
    /// even in JSON format.
    after_output: bool,
    /// The output on stdout is a complete, valid result, only shorter than
    /// the full answer.
    warning: bool,
}

impl Failure {
    fn new(code: u8, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind,
            message: message.into(),
            after_output: false,
            warning: false,
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
        Command::Serve { config } => serve(config.as_deref()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            match format {
                _ if failure.warning => eprintln!("warning: {}", failure.message),
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

    match format {
        Format::Json => {
            // `iri` stays absolute, so a reader needs no prefix handling.
            let entries = |terms: &[(&str, Vec<&TriplesMapId>)]| -> Vec<Value> {
                terms
                    .iter()
                    .map(|(iri, ids)| {
                        json!({
                            "iri": iri,
                            "prefixed_name": mapping.compact_iri(iri),
                            "triples_maps": ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
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
        Format::Table => {
            for (prefix, namespace) in &used {
                println!("PREFIX {prefix}: <{namespace}>");
            }
            if !used.is_empty() {
                println!();
            }
            let rows: Vec<[String; 3]> = classes
                .iter()
                .map(|t| ("class", t))
                .chain(properties.iter().map(|t| ("property", t)))
                .map(|(kind, (iri, ids))| {
                    let ids: Vec<_> = ids.iter().map(|id| id.to_string()).collect();
                    let term = mapping
                        .compact_iri(iri)
                        .unwrap_or_else(|| format!("<{iri}>"));
                    [kind.to_owned(), term, ids.join(", ")]
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
        let before_output = |e| query_failure(e, 0);
        let database = Database::connect(url, limits)
            .await
            .map_err(before_output)?;
        let mut solutions = database.execute(&plan).await.map_err(before_output)?;
        // Waiting for the first row before printing anything means an error
        // in planning or early execution is still reported in `format`.
        let first = solutions.try_next().await.map_err(before_output)?;
        match format {
            Format::Json => print_json(&mut solutions, first).await?,
            Format::Table => print_solutions_table(&mut solutions, first).await?,
        };
        if solutions.is_truncated() {
            return Err(row_limit_warning(limits.row_limit));
        }
        Ok(())
    })
}

fn serve(config_path: Option<&Path>) -> Result<(), Failure> {
    init_logging();
    let config = Config::load(config_path).map_err(server_failure)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::new(EXIT_INPUT, "runtime", e.to_string()))?;
    runtime
        .block_on(v12n_server::serve(config))
        .map_err(server_failure)
}

/// `RUST_LOG` takes `info`, `v12n_server=debug,warn` and the like.
fn init_logging() {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::prelude::*;
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|value| value.parse::<Targets>().ok())
        .unwrap_or_else(|| Targets::new().with_default(LevelFilter::INFO));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_ansi(false),
        )
        .with(filter)
        .init();
}

fn server_failure(error: v12n_server::Error) -> Failure {
    use v12n_server::Error;
    match error {
        Error::Config(_) | Error::Io(_) => Failure::new(EXIT_INPUT, "input", error.to_string()),
        Error::Mapping { .. } => Failure::new(EXIT_MAPPING, "mapping", error.to_string()),
        Error::Database(e) => database_failure(e),
    }
}

fn database_failure(error: v12n_postgres::Error) -> Failure {
    use v12n_postgres::Error;
    let (code, kind) = match &error {
        Error::Config(_) => (EXIT_INPUT, "input"),
        Error::Database(_) => (EXIT_DATABASE, "database"),
        Error::Timeout(_) => (EXIT_TIMEOUT, "timeout"),
        // A comparison the column types make impossible to do right in SQL.
        Error::Core(v12n_core::Error::UnsupportedFeature(_)) => (EXIT_INPUT, "unsupported"),
        Error::Core(_) => (EXIT_DATABASE, "data"),
    };
    Failure::new(code, kind, error.to_string())
}

/// Like [`database_failure`], with a timeout message that names the limit,
/// how far the result got, and the options that change it.
fn query_failure(error: v12n_postgres::Error, rows: u64) -> Failure {
    match error {
        v12n_postgres::Error::Timeout(timeout) => {
            let progress = match rows {
                0 => "before the first row".to_owned(),
                n => format!("after {n} rows"),
            };
            Failure::new(
                EXIT_TIMEOUT,
                "timeout",
                format!(
                    "statement timeout of {timeout:?} reached {progress}; the query was cancelled \
                     (raise --timeout or {}, 0 disables it)",
                    env::STATEMENT_TIMEOUT
                ),
            )
        }
        error => database_failure(error),
    }
}

fn row_limit_warning(row_limit: u64) -> Failure {
    Failure {
        after_output: true,
        warning: true,
        ..Failure::new(
            EXIT_ROW_LIMIT,
            "row_limit",
            format!(
                "result truncated at the row limit of {row_limit}; more rows exist \
                 (raise --row-limit or {}, 0 disables it, or page with LIMIT/OFFSET)",
                env::ROW_LIMIT
            ),
        )
    }
}

/// Streams the solutions as SPARQL 1.1 Query Results JSON.
async fn print_json(
    solutions: &mut Solutions,
    first: Option<v12n_core::solution::Solution>,
) -> Result<(), Failure> {
    let output_failure = |e: io::Error| Failure {
        after_output: true,
        ..Failure::new(EXIT_INPUT, "output", format!("cannot write output: {e}"))
    };
    let stdout = BufWriter::new(io::stdout().lock());
    let mut writer =
        JsonResultsWriter::start(stdout, solutions.variables()).map_err(output_failure)?;
    let mut count: u64 = 0;
    let mut next = first;
    while let Some(solution) = next {
        match writer.write(&solution) {
            Ok(()) => count += 1,
            // The reader went away, e.g. `| head`; nothing left to report to.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
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
                    ..query_failure(e, count)
                });
            }
        };
    }
    match writer.finish().and_then(|mut out| out.flush()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(output_failure(e)),
        _ => Ok(()),
    }
}

/// Prints the solutions as an aligned table, terms in N-Triples syntax and
/// unbound variables as empty cells. Alignment needs every row first; the
/// row limit bounds how many that is.
async fn print_solutions_table(
    solutions: &mut Solutions,
    first: Option<v12n_core::solution::Solution>,
) -> Result<(), Failure> {
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
        next = solutions
            .try_next()
            .await
            .map_err(|e| query_failure(e, rows.len() as u64))?;
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
        _ => Ok(()),
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

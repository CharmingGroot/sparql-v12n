// SPDX-License-Identifier: Apache-2.0
//! `v12n`: check mappings, list terms, explain and run SPARQL, serve HTTP.

use std::ffi::OsStr;
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use v12n_core::backend::{BackendError, Connector, DEFAULT_ROW_LIMIT, ErrorKind, SolutionStream};
use v12n_core::mapping::{BaseIri, Mapping, TermType, TriplesMapId};
use v12n_core::results::{ResultsFormat, ResultsWriter, write_csv_record};
use v12n_core::solution::Solution;
use v12n_core::sql::SqlPlan;
use v12n_core::{Engine, Limits, Planner};
use v12n_postgres::PostgresConnector;
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

const MAIN_HELP: &str = concat!(
    "Typical loop: `terms` lists what a mapping exposes, `explain` shows the
SQL a query becomes without a database, `query` runs it. Every command runs
non-interactively and never prompts.

Output: --format json, csv or table, and for `query` also xml or tsv; json
when stdout is not a terminal, table when it is. Colours only on a terminal,
and never with NO_COLOR set.

Errors: with --format json, an error before any output is one JSON object
on stdout, {\"ok\":false,\"error\":{\"kind\":...,\"message\":...,\"exit_code\":...}},
with \"details\" for some kinds. In the other formats, and once output has
started, an error is one \"error: ...\" line on stderr.

",
    exit_codes_help!()
);

const CHECK_HELP: &str = "Examples (from the repository root):
  v12n check examples/shop/mapping.ttl
  cat examples/shop/mapping.ttl | v12n check - --format json

JSON: {\"ok\":true,\"triples_maps\":N,\"classes\":N,\"properties\":N}.
CSV: one record headed triples_maps,classes,properties.

With --db postgresql://reader@localhost/shop, every table and column the
mapping reads must also exist and be readable. A missing one is an invalid mapping of kind \"catalog\", with
details.problems listing each. V12N_DATABASE_URL is not read here, so that
a check without --db never connects.

Exit codes: 0 valid, 1 invalid input, 2 invalid mapping, 3 database error.";

const TERMS_HELP: &str = "Examples (from the repository root):
  v12n terms examples/shop/mapping.ttl
  v12n terms examples/shop/mapping.ttl --format json
  cat examples/shop/mapping.ttl | v12n terms - --format csv

JSON: {\"prefixes\":{prefix:namespace},\"classes\":[TERM],\"properties\":[TERM]}
with TERM = {\"iri\":...,\"prefixed_name\":... or null,\"triples_maps\":[...]};
the prefixes are those the prefixed names use.
CSV: one record per term headed kind,iri,prefixed_name,triples_maps; kind is
class or property, triples maps are separated by spaces.

Exit codes: 0 success, 1 invalid input, 2 invalid mapping.";

const EXPLAIN_HELP: &str = "Examples (from the repository root):
  v12n explain examples/shop/mapping.ttl examples/shop/queries/vip-orders.rq
  v12n explain examples/shop/mapping.ttl --query 'SELECT ?c WHERE { ?c a <http://example.org/Customer> }' --format json
  cat examples/shop/queries/customers.rq | v12n explain examples/shop/mapping.ttl - --format csv

JSON: {\"sql\":...,\"params\":[...],\"variables\":[{\"name\":...,\"term_type\":...,\"columns\":[...]}]};
term_type is iri, blank_node or literal, columns are the SQL output columns.
CSV: one record, the SQL and then one field per bind parameter, headed
sql,$1,$2,...

Without a database the SQL keeps duplicate-removing steps that table keys
would make unnecessary, so `query` may run simpler SQL.

Exit codes: 0 success, 1 invalid input, query syntax or unsupported SPARQL,
2 invalid mapping.";

const QUERY_HELP: &str = concat!(
    "Examples (from the repository root, with a database such as testenv/):
  v12n query examples/shop/mapping.ttl examples/shop/queries/vip-orders.rq --db postgresql://reader@localhost/shop
  V12N_DATABASE_URL=postgresql://reader@localhost/shop v12n query examples/shop/mapping.ttl --query 'SELECT ?c WHERE { ?c a <http://example.org/Customer> }' --format csv
  cat examples/shop/queries/customers.rq | v12n query examples/shop/mapping.ttl - --format table

Formats: json is SPARQL 1.1 Query Results JSON, xml the SPARQL Query
Results XML Format, csv and tsv SPARQL 1.1 Query Results CSV and TSV. CSV
holds lexical forms only, TSV holds terms in SPARQL syntax, and both write
an unbound variable as an empty field. All four are written as rows arrive.
The session is read-only; still, connect as a user that can only read.

When more rows exist than --row-limit, the first --row-limit solutions are
printed as a complete result, a warning naming the limit goes to stderr,
and the command exits with 5. A result of exactly --row-limit rows is not
truncated and exits with 0.

When the statement timeout cancels the query, stderr names the timeout and
the command exits with 4. Before the first row, the error is reported in
--format like other errors; after output has started, JSON and XML on
stdout are left incomplete so that they cannot pass for a complete result.
CSV and TSV have no end marker: check the exit code.

",
    exit_codes_help!()
);

const SERVE_HELP: &str = "Examples (from the repository root):
  v12n serve --config testenv/v12n.toml
  V12N_DATABASE_URL=postgresql://reader@localhost/shop V12N_MAPPING=examples/shop/mapping.ttl v12n serve
  curl -H 'Accept: application/sparql-results+json' --data-urlencode query@examples/shop/queries/vip-orders.rq http://127.0.0.1:7878/sparql

Settings (file key, environment variable, default):
  [server] address                  V12N_ADDRESS                  127.0.0.1
  [server] port                     V12N_PORT                     7878
  [database] url                    V12N_DATABASE_URL             required
  [database] password_env           (file only)                   none
  [mapping] path                    V12N_MAPPING                  required
  [mapping] base_iri                V12N_BASE_IRI                 none (--base-iri)
  [limits] statement_timeout        V12N_STATEMENT_TIMEOUT        30s (0: none)
  [limits] row_limit                V12N_ROW_LIMIT                10000 (0: none)
  [limits] max_query_bytes          V12N_MAX_QUERY_BYTES          65536
  [limits] max_concurrent_requests  V12N_MAX_CONCURRENT_REQUESTS  16

Environment variables override the file. Other V12N_* variables are
ignored with a warning in the log.

Without a base IRI, a relative IRI from a column or template is a data error.
A relative mapping path in the file is relative to the file. Connect as a
database user that can only read; sessions are also set read-only.
Logs go to stderr; RUST_LOG sets the level (default info).

Exit codes: 1 invalid settings or address in use, 2 invalid mapping,
3 database unreachable at startup.";

#[derive(Parser)]
#[command(
    name = "v12n",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("V12N_COMMIT"), ")"),
    about = "Query PostgreSQL with SPARQL through R2RML mappings",
    after_help = MAIN_HELP
)]
struct Cli {
    /// Output format; xml and tsv are for `query` only [default: table on a
    /// terminal, json otherwise]
    #[arg(long, global = true, value_enum)]
    format: Option<Format>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Json,
    Xml,
    Csv,
    Tsv,
    Table,
}

impl Format {
    /// The SPARQL results format that `query` writes, if any.
    fn results(self) -> Option<ResultsFormat> {
        match self {
            Format::Json => Some(ResultsFormat::Json),
            Format::Xml => Some(ResultsFormat::Xml),
            Format::Csv => Some(ResultsFormat::Csv),
            Format::Tsv => Some(ResultsFormat::Tsv),
            Format::Table => None,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Check a mapping for syntax and reference errors
    #[command(after_help = CHECK_HELP)]
    Check {
        /// R2RML mapping in Turtle, or `-` for stdin
        mapping: PathBuf,
        /// Also check the tables and columns the mapping reads on this
        /// PostgreSQL database, e.g. postgresql://reader@localhost/shop
        #[arg(long, value_name = "URL")]
        db: Option<String>,
    },
    /// List the classes and properties a mapping exposes
    #[command(after_help = TERMS_HELP)]
    Terms {
        /// R2RML mapping in Turtle, or `-` for stdin
        mapping: PathBuf,
    },
    /// Print the SQL a SPARQL query unfolds to, without a database
    #[command(
        after_help = EXPLAIN_HELP,
        override_usage = "v12n explain [OPTIONS] <MAPPING> <QUERY|--query <SPARQL>>"
    )]
    Explain {
        /// R2RML mapping in Turtle, or `-` for stdin
        mapping: PathBuf,
        #[command(flatten)]
        query: QueryArg,
        #[command(flatten)]
        base_iri: BaseIriArg,
    },
    /// Run a SPARQL query on PostgreSQL and print the solutions
    #[command(
        after_help = QUERY_HELP,
        override_usage = "v12n query [OPTIONS] <MAPPING> <QUERY|--query <SPARQL>>"
    )]
    Query {
        /// R2RML mapping in Turtle, or `-` for stdin
        mapping: PathBuf,
        #[command(flatten)]
        query: QueryArg,
        /// PostgreSQL connection URL, e.g. postgresql://reader@localhost/shop
        #[arg(long, env = env::DATABASE_URL, hide_env_values = true)]
        db: Option<String>,
        /// Most solutions to return; 0 means no limit
        #[arg(long, env = env::ROW_LIMIT, default_value_t = DEFAULT_ROW_LIMIT)]
        row_limit: u64,
        /// Statement timeout such as 500ms, 30s or 2m; 0 disables it
        #[arg(long, env = env::STATEMENT_TIMEOUT, default_value = "30s", value_parser = parse_duration)]
        timeout: Duration,
        #[command(flatten)]
        base_iri: BaseIriArg,
    },
    /// Serve the SPARQL 1.1 Protocol endpoint at /sparql
    #[command(after_help = SERVE_HELP)]
    Serve {
        /// TOML settings file; the environment variables below override it
        #[arg(long)]
        config: Option<PathBuf>,
        /// Base IRI for relative IRIs from columns and templates; overrides
        /// [mapping] base_iri and V12N_BASE_IRI
        #[arg(long, value_parser = parse_base_iri)]
        base_iri: Option<BaseIri>,
    },
}

/// The query, from a file, stdin or the command line; exactly one.
#[derive(clap::Args)]
#[group(required = true, multiple = false)]
struct QueryArg {
    /// SPARQL SELECT query file, or `-` for stdin
    #[arg(value_name = "QUERY")]
    file: Option<PathBuf>,
    /// SPARQL SELECT query text, instead of a QUERY file
    #[arg(long = "query", value_name = "SPARQL")]
    text: Option<String>,
}

#[derive(clap::Args)]
struct BaseIriArg {
    /// Base IRI that relative IRIs from columns and templates resolve
    /// against, by prepending it (R2RML); without it they are data errors
    #[arg(long = "base-iri", env = env::BASE_IRI, value_parser = parse_base_iri)]
    base_iri: Option<BaseIri>,
}

fn parse_base_iri(value: &str) -> Result<BaseIri, String> {
    BaseIri::new(value).map_err(|e| e.to_string())
}

/// How results go to stdout.
#[derive(Clone, Copy)]
struct Output {
    format: Format,
    /// Whether to use ANSI styles on stdout (table format only).
    color: bool,
}

/// A failure to report: exit code, error kind for JSON output, message.
struct Failure {
    code: u8,
    kind: &'static str,
    message: String,
    /// Kind-specific fields for JSON output, such as the file that could
    /// not be read or the timeout that was reached.
    details: Option<Value>,
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
            details: None,
            after_output: false,
            warning: false,
        }
    }

    fn to_json(&self) -> Value {
        let mut error = json!({
            "kind": self.kind,
            "message": self.message,
            "exit_code": self.code,
        });
        if let Some(details) = &self.details {
            error["details"] = details.clone();
        }
        json!({ "ok": false, "error": error })
    }
}

const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[1;31m";
const YELLOW: &str = "\x1b[1;33m";
const RESET: &str = "\x1b[0m";

/// Colours only on a terminal, unless `NO_COLOR` is set to anything
/// non-empty (no-color.org) or `TERM` is `dumb`.
fn wants_color(is_terminal: bool, no_color: Option<&OsStr>, term: Option<&OsStr>) -> bool {
    is_terminal && no_color.is_none_or(OsStr::is_empty) && term.is_none_or(|term| term != "dumb")
}

fn stream_color(is_terminal: bool) -> bool {
    wants_color(
        is_terminal,
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
    )
}

fn paint(color: bool, style: &str, text: &str) -> String {
    if color {
        format!("{style}{text}{RESET}")
    } else {
        text.to_owned()
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
    let stdout_is_terminal = io::stdout().is_terminal();
    let output = Output {
        format: cli.format.unwrap_or(if stdout_is_terminal {
            Format::Table
        } else {
            Format::Json
        }),
        color: stream_color(stdout_is_terminal),
    };
    let result = match &cli.command {
        // XML and TSV are formats for SPARQL results only.
        Command::Check { .. } | Command::Terms { .. } | Command::Explain { .. }
            if matches!(output.format, Format::Xml | Format::Tsv) =>
        {
            let name = output.format.to_possible_value().unwrap_or_default();
            Err(Failure::new(
                EXIT_INPUT,
                "input",
                format!(
                    "--format {} is for `query` only; use json, csv or table",
                    name.get_name()
                ),
            ))
        }
        Command::Check { mapping, db } => check(
            mapping,
            db.as_deref().map(|url| connector(url, None)),
            output,
        ),
        Command::Terms { mapping } => terms(mapping, output),
        Command::Explain {
            mapping,
            query,
            base_iri,
        } => explain(mapping, query, base_iri.base_iri.as_ref(), output),
        Command::Query {
            mapping,
            query: query_arg,
            db,
            row_limit,
            timeout,
            base_iri,
        } => query(
            mapping,
            query_arg,
            base_iri.base_iri.as_ref(),
            db.as_deref().map(|url| connector(url, None)),
            Limits {
                statement_timeout: *timeout,
                row_limit: *row_limit,
            },
            output,
        ),
        Command::Serve { config, base_iri } => serve(config.as_deref(), base_iri.clone()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            let color = stream_color(io::stderr().is_terminal());
            if failure.warning {
                eprintln!("{} {}", paint(color, YELLOW, "warning:"), failure.message);
            } else if failure.after_output || output.format != Format::Json {
                eprintln!("{} {}", paint(color, RED, "error:"), failure.message);
            } else {
                println!("{}", failure.to_json());
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

fn load_mapping(path: &Path) -> Result<Mapping, Failure> {
    let text = read_input(path)?;
    Mapping::from_turtle(&text).map_err(|e| Failure::new(EXIT_MAPPING, "mapping", e.to_string()))
}

fn output_failure(error: io::Error) -> Failure {
    Failure::new(
        EXIT_INPUT,
        "output",
        format!("cannot write output: {error}"),
    )
}

/// Writes CSV records to stdout; a reader that went away is not an error.
fn print_csv<S: AsRef<str>>(records: &[Vec<S>]) -> Result<(), Failure> {
    let mut out = BufWriter::new(io::stdout().lock());
    let result = records
        .iter()
        .try_for_each(|record| write_csv_record(&mut out, record))
        .and_then(|()| out.flush());
    match result {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(output_failure(e)),
        _ => Ok(()),
    }
}

fn check(path: &Path, connector: Option<impl Connector>, output: Output) -> Result<(), Failure> {
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
            Engine::new(Arc::clone(planner), backend)
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

fn terms(path: &Path, output: Output) -> Result<(), Failure> {
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

/// Reads the mapping and the query and unfolds the query.
fn load_plan(
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

fn explain(
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

fn query(
    mapping_path: &Path,
    query_arg: &QueryArg,
    base_iri: Option<&BaseIri>,
    connector: Option<impl Connector>,
    limits: Limits,
    output: Output,
) -> Result<(), Failure> {
    let (planner, plan) = load_plan(mapping_path, query_arg, base_iri)?;
    let connector = connector.ok_or_else(|| {
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
        let before_output = |e| query_failure(&e, 0);
        let backend = connector.connect(limits).await.map_err(before_output)?;
        let engine = Engine::new(Arc::new(planner), backend);
        let mut solutions = engine.execute(&plan).await.map_err(before_output)?;
        // Waiting for the first row before printing anything means an error
        // in planning or early execution is still reported in `format`.
        let first = solutions.try_next().await.map_err(before_output)?;
        match output.format.results() {
            Some(format) => {
                let stdout = BufWriter::new(io::stdout().lock());
                let writer = ResultsWriter::start(format, stdout, solutions.variables())
                    .map_err(output_failure)?;
                stream_solutions(&mut solutions, first, writer).await?
            }
            None => print_solutions_table(&mut solutions, first, output.color).await?,
        };
        if solutions.is_truncated() {
            return Err(row_limit_warning(limits.row_limit));
        }
        Ok(())
    })
}

fn serve(config_path: Option<&Path>, base_iri: Option<BaseIri>) -> Result<(), Failure> {
    init_logging();
    let mut config = Config::load(config_path).map_err(server_failure)?;
    if base_iri.is_some() {
        config.base_iri = base_iri;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::new(EXIT_INPUT, "runtime", e.to_string()))?;
    let connector = connector(&config.database.url, config.database.password.as_deref());
    runtime
        .block_on(v12n_server::serve(config, connector))
        .map_err(server_failure)
}

/// The backend for a database URL. PostgreSQL is the only one, and it
/// also takes libpq `key=value` strings, which have no scheme to tell
/// databases apart by.
fn connector(url: &str, password: Option<&str>) -> PostgresConnector {
    let connector = PostgresConnector::new(url);
    match password {
        Some(password) => connector.with_password(password),
        None => connector,
    }
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
        Error::Database(e) => database_failure(e.as_ref()),
    }
}

fn database_failure(error: &dyn BackendError) -> Failure {
    let (code, kind) = match error.kind() {
        ErrorKind::Config => (EXIT_INPUT, "input"),
        ErrorKind::Database => (EXIT_DATABASE, "database"),
        ErrorKind::Timeout(_) => (EXIT_TIMEOUT, "timeout"),
        // A comparison the column types make impossible to do right in SQL.
        ErrorKind::Core(v12n_core::Error::UnsupportedFeature(_)) => (EXIT_INPUT, "unsupported"),
        ErrorKind::Core(_) => (EXIT_DATABASE, "data"),
    };
    Failure::new(code, kind, error.to_string())
}

/// Like [`database_failure`], with a timeout message that names the limit,
/// how far the result got, and the options that change it.
fn query_failure(error: &dyn BackendError, rows: u64) -> Failure {
    match error.kind() {
        ErrorKind::Timeout(timeout) => {
            let progress = match rows {
                0 => "before the first row".to_owned(),
                n => format!("after {n} rows"),
            };
            Failure {
                details: Some(json!({
                    "timeout_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    "rows": rows,
                })),
                ..Failure::new(
                    EXIT_TIMEOUT,
                    "timeout",
                    format!(
                        "statement timeout of {timeout:?} reached {progress}; the query was cancelled \
                         (raise --timeout or {}, 0 disables it)",
                        env::STATEMENT_TIMEOUT
                    ),
                )
            }
        }
        _ => database_failure(error),
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

/// Streams the solutions into `writer` as they arrive.
async fn stream_solutions(
    solutions: &mut impl SolutionStream,
    first: Option<Solution>,
    mut writer: ResultsWriter<impl Write>,
) -> Result<(), Failure> {
    let after_output = |failure: Failure| Failure {
        after_output: true,
        ..failure
    };
    let mut count: u64 = 0;
    let mut next = first;
    while let Some(solution) = next {
        match writer.write(&solution) {
            Ok(()) => count += 1,
            // The reader went away, e.g. `| head`; nothing left to report to.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => return Err(after_output(output_failure(e))),
        }
        next = match solutions.try_next().await {
            Ok(next) => next,
            Err(e) => {
                // Partial JSON and XML stay unterminated so they cannot
                // pass for a complete result.
                let _ = writer.get_mut().flush();
                return Err(after_output(query_failure(&e, count)));
            }
        };
    }
    match writer.finish().and_then(|mut out| out.flush()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(after_output(output_failure(e))),
        _ => Ok(()),
    }
}

/// Prints the solutions as an aligned table, terms in N-Triples syntax and
/// unbound variables as empty cells. Alignment needs every row first; the
/// row limit bounds how many that is.
async fn print_solutions_table(
    solutions: &mut impl SolutionStream,
    first: Option<Solution>,
    color: bool,
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
            .map_err(|e| query_failure(&e, rows.len() as u64))?;
    }
    let header: Vec<String> = solutions
        .variables()
        .iter()
        .map(|v| format!("?{v}"))
        .collect();
    let mut out = BufWriter::new(io::stdout().lock());
    let result = write_table(&mut out, &header, &rows, color).and_then(|()| out.flush());
    match result {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(output_failure(e)),
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

fn print_table<const N: usize>(header: [&str; N], rows: &[[String; N]], color: bool) {
    let header: Vec<String> = header.iter().map(|h| (*h).to_owned()).collect();
    let mut out = BufWriter::new(io::stdout().lock());
    // A reader that went away, e.g. `| head`, is not an error.
    let _ = write_table(&mut out, &header, rows, color).and_then(|()| out.flush());
}

/// Writes left-aligned columns two spaces apart, the header in bold when
/// `color` is set.
fn write_table<R: AsRef<[String]>>(
    out: &mut impl Write,
    header: &[String],
    rows: &[R],
    color: bool,
) -> io::Result<()> {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row.as_ref()) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, &width)| format!("{cell:<width$}"))
            .collect();
        padded.join("  ").trim_end().to_owned()
    };
    writeln!(out, "{}", paint(color, BOLD, &line(header)))?;
    for row in rows {
        writeln!(out, "{}", line(row.as_ref()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn color_needs_a_terminal_and_no_no_color() {
        let set = Some(OsStr::new("1"));
        let empty = Some(OsStr::new(""));
        let xterm = Some(OsStr::new("xterm-256color"));
        assert!(wants_color(true, None, xterm));
        assert!(wants_color(true, None, None));
        assert!(!wants_color(false, None, xterm));
        assert!(!wants_color(true, set, xterm));
        // no-color.org: only a non-empty value disables colour.
        assert!(wants_color(true, empty, xterm));
        assert!(!wants_color(true, None, Some(OsStr::new("dumb"))));
    }

    /// Splits a shell command line on unquoted spaces and `|`, keeping the
    /// last pipeline stage; enough for the examples in the help texts.
    fn last_stage(line: &str) -> Vec<String> {
        let mut stages: Vec<Vec<String>> = vec![Vec::new()];
        let mut word: Option<String> = None;
        let mut quote: Option<char> = None;
        for c in line.chars() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), c) => word.get_or_insert_default().push(c),
                (None, '\'' | '"') => {
                    quote = Some(c);
                    word.get_or_insert_default();
                }
                (None, ' ') => stages.last_mut().unwrap().extend(word.take()),
                (None, '|') => {
                    stages.last_mut().unwrap().extend(word.take());
                    stages.push(Vec::new());
                }
                (None, c) => word.get_or_insert_default().push(c),
            }
        }
        assert!(quote.is_none(), "unbalanced quotes: {line}");
        stages.last_mut().unwrap().extend(word.take());
        stages.pop().unwrap()
    }

    #[test]
    fn every_help_example_parses() {
        let mut command = Cli::command();
        command.build();
        let mut checked = 0;
        for sub in command.get_subcommands().filter(|s| s.get_name() != "help") {
            let help = sub
                .get_after_help()
                .map(ToString::to_string)
                .unwrap_or_default();
            let examples: Vec<&str> = help
                .lines()
                .skip_while(|l| !l.starts_with("Examples"))
                .skip(1)
                .take_while(|l| l.starts_with("  "))
                .collect();
            assert!(
                (2..=3).contains(&examples.len()),
                "{}: {examples:?}",
                sub.get_name()
            );
            for example in examples {
                let words: Vec<String> = last_stage(example.trim())
                    .into_iter()
                    .skip_while(|w| w.contains('=') && !w.starts_with('-'))
                    .collect();
                if words.first().map(String::as_str) != Some("v12n") {
                    continue;
                }
                assert_eq!(words[1], sub.get_name(), "{example}");
                if let Err(e) = Cli::try_parse_from(&words) {
                    panic!("{example}\n{e}");
                }
                checked += 1;
            }
        }
        assert!(checked >= 10, "{checked}");
    }

    #[test]
    fn query_is_a_file_or_text_but_not_both() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|_| ());
        assert!(parse(&["v12n", "explain", "m.ttl", "q.rq"]).is_ok());
        assert!(parse(&["v12n", "explain", "m.ttl", "--query", "SELECT * {}"]).is_ok());
        assert!(parse(&["v12n", "explain", "m.ttl"]).is_err());
        assert!(parse(&["v12n", "explain", "m.ttl", "q.rq", "--query", "SELECT * {}"]).is_err());
    }
}

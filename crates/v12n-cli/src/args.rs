// SPDX-License-Identifier: Apache-2.0
//! Command-line arguments and the help texts.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use v12n_core::backend::DEFAULT_ROW_LIMIT;
use v12n_core::mapping::BaseIri;
use v12n_core::results::ResultsFormat;
use v12n_server::config::{env, parse_duration};

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

pub(crate) const MAIN_HELP: &str = concat!(
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

pub(crate) const CHECK_HELP: &str = "Examples (from the repository root):
  v12n check examples/shop/mapping.ttl
  cat examples/shop/mapping.ttl | v12n check - --format json

JSON: {\"ok\":true,\"triples_maps\":N,\"classes\":N,\"properties\":N}.
CSV: one record headed triples_maps,classes,properties.

With --db postgresql://reader@localhost/shop, every table and column the
mapping reads must also exist and be readable. A missing one is an invalid mapping of kind \"catalog\", with
details.problems listing each. V12N_DATABASE_URL is not read here, so that
a check without --db never connects.

Exit codes: 0 valid, 1 invalid input, 2 invalid mapping, 3 database error.";

pub(crate) const TERMS_HELP: &str = "Examples (from the repository root):
  v12n terms examples/shop/mapping.ttl
  v12n terms examples/shop/mapping.ttl --format json
  cat examples/shop/mapping.ttl | v12n terms - --format csv

JSON: {\"prefixes\":{prefix:namespace},\"classes\":[TERM],\"properties\":[TERM]}
with TERM = {\"iri\":...,\"prefixed_name\":... or null,\"triples_maps\":[...]};
the prefixes are those the prefixed names use.
CSV: one record per term headed kind,iri,prefixed_name,triples_maps; kind is
class or property, triples maps are separated by spaces.

Exit codes: 0 success, 1 invalid input, 2 invalid mapping.";

pub(crate) const EXPLAIN_HELP: &str = "Examples (from the repository root):
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

pub(crate) const QUERY_HELP: &str = concat!(
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

pub(crate) const SERVE_HELP: &str = "Examples (from the repository root):
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
pub(crate) struct Cli {
    /// Output format; xml and tsv are for `query` only [default: table on a
    /// terminal, json otherwise]
    #[arg(long, global = true, value_enum)]
    pub(crate) format: Option<Format>,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    Json,
    Xml,
    Csv,
    Tsv,
    Table,
}

impl Format {
    /// The SPARQL results format that `query` writes, if any.
    pub(crate) fn results(self) -> Option<ResultsFormat> {
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
pub(crate) enum Command {
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
pub(crate) struct QueryArg {
    /// SPARQL SELECT query file, or `-` for stdin
    #[arg(value_name = "QUERY")]
    pub(crate) file: Option<PathBuf>,
    /// SPARQL SELECT query text, instead of a QUERY file
    #[arg(long = "query", value_name = "SPARQL")]
    pub(crate) text: Option<String>,
}

#[derive(clap::Args)]
pub(crate) struct BaseIriArg {
    /// Base IRI that relative IRIs from columns and templates resolve
    /// against, by prepending it (R2RML); without it they are data errors
    #[arg(long = "base-iri", env = env::BASE_IRI, value_parser = parse_base_iri)]
    pub(crate) base_iri: Option<BaseIri>,
}

pub(crate) fn parse_base_iri(value: &str) -> Result<BaseIri, String> {
    BaseIri::new(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

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

// SPDX-License-Identifier: Apache-2.0
//! `v12n`: check mappings, list terms, explain and run SPARQL, serve HTTP.

mod args;
mod command;
mod failure;
mod input;
mod output;

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use v12n_core::Limits;
use v12n_postgres::PostgresConnector;

use args::{Cli, Command, Format};
use command::{check::check, explain::explain, query::query, serve::serve, terms::terms};
use failure::{EXIT_INPUT, Failure};
use output::{Output, RED, YELLOW, paint, stream_color};

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

/// The backend for a database URL. PostgreSQL is the only one, and it
/// also takes libpq `key=value` strings, which have no scheme to tell
/// databases apart by.
pub(crate) fn connector(url: &str, password: Option<&str>) -> PostgresConnector {
    let connector = PostgresConnector::new(url);
    match password {
        Some(password) => connector.with_password(password),
        None => connector,
    }
}

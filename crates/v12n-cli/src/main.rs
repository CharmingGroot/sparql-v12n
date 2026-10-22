// SPDX-License-Identifier: Apache-2.0
//! `v12n`: check mappings, list terms, explain and run SPARQL, serve HTTP.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "v12n",
    version,
    about = "Query PostgreSQL with SPARQL through R2RML mappings"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check a mapping for syntax and reference errors
    Check { mapping: PathBuf },
    /// List the classes and properties a mapping exposes
    Terms { mapping: PathBuf },
    /// Print the SQL a SPARQL query unfolds to, without a database
    Explain { mapping: PathBuf, query: PathBuf },
    /// Run a SPARQL query against the database
    Query { mapping: PathBuf, query: PathBuf },
    /// Serve the SPARQL 1.1 Protocol endpoint
    Serve,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let name = match cli.command {
        Command::Check { .. } => "check",
        Command::Terms { .. } => "terms",
        Command::Explain { .. } => "explain",
        Command::Query { .. } => "query",
        Command::Serve => "serve",
    };
    eprintln!("v12n {name}: not implemented yet");
    ExitCode::from(1)
}

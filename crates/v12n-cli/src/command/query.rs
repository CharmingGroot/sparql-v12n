// SPDX-License-Identifier: Apache-2.0
//! `v12n query`: run a query and print its solutions.

use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use v12n_core::backend::{Connector, SolutionStream};
use v12n_core::mapping::BaseIri;
use v12n_core::results::{ResultsFormat, ResultsWriter, write_boolean};
use v12n_core::solution::Solution;
use v12n_core::sparql::QueryForm;
use v12n_core::{Engine, Limits};

use crate::args::QueryArg;
use crate::failure::{
    EXIT_DATABASE, EXIT_INPUT, Failure, output_failure, query_failure, row_limit_warning,
};
use crate::input::load_plan;
use crate::output::{Output, write_table};

pub(crate) fn query(
    mapping_path: &Path,
    query_arg: &QueryArg,
    base_iri: Option<&BaseIri>,
    connector: Option<impl Connector>,
    limits: Limits,
    output: Output,
) -> Result<(), Failure> {
    let (planner, plan) = load_plan(mapping_path, query_arg, base_iri)?;
    let ask = plan.form == QueryForm::Ask;
    if let Some(format) = output.format.results()
        && ask
        && !ResultsFormat::BOOLEAN.contains(&format)
    {
        return Err(Failure::new(
            EXIT_INPUT,
            "input",
            "csv and tsv have no form for ASK results; use --format json, xml or table",
        ));
    }
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
        // An empty catalog: the query reads the entries of its own tables.
        let engine = Engine::new(Arc::new(planner), Arc::default(), backend);
        let mut solutions = engine.execute(&plan).await.map_err(before_output)?;
        // Waiting for the first row before printing anything means an error
        // in planning or early execution is still reported in `format`.
        let first = solutions.try_next().await.map_err(before_output)?;
        if ask {
            return print_boolean(output.format.results(), first.is_some());
        }
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

/// Prints the answer of an `ASK` query, as a bare `true` or `false` line
/// in the table format.
fn print_boolean(format: Option<ResultsFormat>, value: bool) -> Result<(), Failure> {
    let mut out = BufWriter::new(io::stdout().lock());
    let result = match format {
        Some(format) => write_boolean(format, &mut out, value),
        None => writeln!(out, "{value}"),
    };
    match result.and_then(|()| out.flush()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(output_failure(e)),
        _ => Ok(()),
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

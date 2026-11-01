// SPDX-License-Identifier: Apache-2.0
//! Output helpers: CSV records, aligned tables and colours.

use std::ffi::OsStr;
use std::io::{self, BufWriter, Write};

use v12n_core::results::write_csv_record;

use crate::args::Format;
use crate::failure::{Failure, output_failure};

/// How results go to stdout.
#[derive(Clone, Copy)]
pub(crate) struct Output {
    pub(crate) format: Format,
    /// Whether to use ANSI styles on stdout (table format only).
    pub(crate) color: bool,
}

pub(crate) const BOLD: &str = "\x1b[1m";
pub(crate) const RED: &str = "\x1b[1;31m";
pub(crate) const YELLOW: &str = "\x1b[1;33m";
pub(crate) const RESET: &str = "\x1b[0m";

/// Colours only on a terminal, unless `NO_COLOR` is set to anything
/// non-empty (no-color.org) or `TERM` is `dumb`.
fn wants_color(is_terminal: bool, no_color: Option<&OsStr>, term: Option<&OsStr>) -> bool {
    is_terminal && no_color.is_none_or(OsStr::is_empty) && term.is_none_or(|term| term != "dumb")
}

pub(crate) fn stream_color(is_terminal: bool) -> bool {
    wants_color(
        is_terminal,
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
    )
}

pub(crate) fn paint(color: bool, style: &str, text: &str) -> String {
    if color {
        format!("{style}{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// Writes CSV records to stdout; a reader that went away is not an error.
pub(crate) fn print_csv<S: AsRef<str>>(records: &[Vec<S>]) -> Result<(), Failure> {
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

pub(crate) fn print_table<const N: usize>(header: [&str; N], rows: &[[String; N]], color: bool) {
    let header: Vec<String> = header.iter().map(|h| (*h).to_owned()).collect();
    let mut out = BufWriter::new(io::stdout().lock());
    // A reader that went away, e.g. `| head`, is not an error.
    let _ = write_table(&mut out, &header, rows, color).and_then(|()| out.flush());
}

/// Writes left-aligned columns two spaces apart, the header in bold when
/// `color` is set.
pub(crate) fn write_table<R: AsRef<[String]>>(
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
}

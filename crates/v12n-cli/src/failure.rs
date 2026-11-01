// SPDX-License-Identifier: Apache-2.0
//! Failures and the exit codes they map to, which are part of the CLI
//! contract.

use std::io;

use serde_json::{Value, json};
use v12n_core::backend::{BackendError, ErrorKind};
use v12n_server::config::env;

/// Exit codes are part of the CLI contract; scripts and agents rely on them.
pub(crate) const EXIT_INPUT: u8 = 1;
pub(crate) const EXIT_MAPPING: u8 = 2;
pub(crate) const EXIT_DATABASE: u8 = 3;
pub(crate) const EXIT_TIMEOUT: u8 = 4;
/// Separate from [`EXIT_TIMEOUT`] because the fix differs: a higher limit
/// or paging with LIMIT/OFFSET, not a faster query or a longer timeout.
pub(crate) const EXIT_ROW_LIMIT: u8 = 5;

/// A failure to report: exit code, error kind for JSON output, message.
pub(crate) struct Failure {
    pub(crate) code: u8,
    pub(crate) kind: &'static str,
    pub(crate) message: String,
    /// Kind-specific fields for JSON output, such as the file that could
    /// not be read or the timeout that was reached.
    pub(crate) details: Option<Value>,
    /// Results were already written to stdout, so the error goes to stderr
    /// even in JSON format.
    pub(crate) after_output: bool,
    /// The output on stdout is a complete, valid result, only shorter than
    /// the full answer.
    pub(crate) warning: bool,
}

impl Failure {
    pub(crate) fn new(code: u8, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind,
            message: message.into(),
            details: None,
            after_output: false,
            warning: false,
        }
    }

    pub(crate) fn to_json(&self) -> Value {
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

pub(crate) fn output_failure(error: io::Error) -> Failure {
    Failure::new(
        EXIT_INPUT,
        "output",
        format!("cannot write output: {error}"),
    )
}

pub(crate) fn server_failure(error: v12n_server::Error) -> Failure {
    use v12n_server::Error;
    match error {
        Error::Config(_) | Error::Io(_) => Failure::new(EXIT_INPUT, "input", error.to_string()),
        Error::Mapping { .. } => Failure::new(EXIT_MAPPING, "mapping", error.to_string()),
        Error::Database(e) => database_failure(e.as_ref()),
    }
}

pub(crate) fn database_failure(error: &dyn BackendError) -> Failure {
    let (code, kind) = match error.kind() {
        ErrorKind::Config => (EXIT_INPUT, "input"),
        ErrorKind::Database => (EXIT_DATABASE, "database"),
        ErrorKind::Timeout(_) => (EXIT_TIMEOUT, "timeout"),
        ErrorKind::StaleCatalog => (EXIT_DATABASE, "database"),
        ErrorKind::Core(v12n_core::Error::Catalog(_)) => (EXIT_DATABASE, "database"),
        // A comparison the column types make impossible to do right in SQL.
        ErrorKind::Core(v12n_core::Error::UnsupportedFeature(_)) => (EXIT_INPUT, "unsupported"),
        ErrorKind::Core(_) => (EXIT_DATABASE, "data"),
    };
    Failure::new(code, kind, error.to_string())
}

/// Like [`database_failure`], with a timeout message that names the limit,
/// how far the result got, and the options that change it.
pub(crate) fn query_failure(error: &dyn BackendError, rows: u64) -> Failure {
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

pub(crate) fn row_limit_warning(row_limit: u64) -> Failure {
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

// SPDX-License-Identifier: Apache-2.0
//! The crate-wide error type.

use std::time::Duration;

use tokio_postgres::error::SqlState;

/// Everything that can go wrong while running a plan.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The connection string cannot be parsed. The message never repeats
    /// the string itself, which may hold a password.
    #[error("invalid database URL: {0}")]
    Config(String),
    /// Connecting failed, or the server reported an error.
    #[error("database error: {0}")]
    Database(String),
    /// The server cancelled the query at `statement_timeout`.
    #[error("query cancelled after the statement timeout of {0:?}")]
    Timeout(Duration),
    /// A row could not be turned into a solution.
    #[error(transparent)]
    Core(#[from] v12n_core::Error),
}

impl Error {
    pub(crate) fn from_postgres(error: tokio_postgres::Error, timeout: Duration) -> Self {
        if let Some(db) = error.as_db_error() {
            if db.code() == &SqlState::QUERY_CANCELED {
                return Self::Timeout(timeout);
            }
            return Self::Database(db.to_string());
        }
        Self::Database(message(&error))
    }

    pub(crate) fn config(error: tokio_postgres::Error) -> Self {
        Self::Config(message(&error))
    }
}

/// The error and its causes on one line; `tokio_postgres::Error` alone
/// prints only its kind, such as "error connecting to server".
fn message(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

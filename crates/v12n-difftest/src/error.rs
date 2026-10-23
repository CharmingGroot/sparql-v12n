// SPDX-License-Identifier: Apache-2.0
//! The crate-wide error type.

use std::path::PathBuf;

/// Everything that makes a differential check fail.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A case directory or one of its files is missing or invalid.
    #[error("{}: {message}", path.display())]
    Case { path: PathBuf, message: String },
    /// The database URL or the Ontop endpoints are not usable.
    #[error("environment: {0}")]
    Environment(String),
    /// Ontop could not answer the query.
    #[error("Ontop: {0}")]
    Ontop(String),
    /// This engine could not answer the query.
    #[error("v12n: {0}")]
    Engine(String),
    /// Both engines answered and the check failed.
    #[error("{0}")]
    Failed(String),
}

impl Error {
    pub(crate) fn case(path: impl Into<PathBuf>, message: impl ToString) -> Self {
        Self::Case {
            path: path.into(),
            message: message.to_string(),
        }
    }
}

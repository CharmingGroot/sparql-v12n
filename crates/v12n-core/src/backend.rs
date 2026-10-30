// SPDX-License-Identifier: Apache-2.0
//! The driven port: what a database adapter implements so that the
//! [`Engine`](crate::Engine) can run plans on it.
//!
//! Methods that wait on the database return `impl Future + Send` instead
//! of being `async fn`, so that an HTTP handler that awaits them stays
//! `Send` and the core needs no async runtime.

use std::future::Future;
use std::time::Duration;

use crate::catalog::{CatalogPlan, CatalogRequest, CatalogSnapshot};
use crate::dialect::Dialect;
use crate::solution::Solution;

/// Default for [`Limits::statement_timeout`].
pub const DEFAULT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default for [`Limits::row_limit`].
pub const DEFAULT_ROW_LIMIT: u64 = 10_000;

/// Resource limits that every backend enforces on every query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Time the database may spend on one statement; zero disables it.
    pub statement_timeout: Duration,
    /// Most solutions returned per query; zero means no limit.
    pub row_limit: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            statement_timeout: DEFAULT_STATEMENT_TIMEOUT,
            row_limit: DEFAULT_ROW_LIMIT,
        }
    }
}

/// One session with a database that runs [`CatalogPlan`]s.
pub trait Backend: Send + Sync {
    /// Ties the SQL syntax to the database, so that a plan cannot be
    /// rendered for one database and run on another.
    type Dialect: Dialect;
    type Solutions: SolutionStream<Error = Self::Error>;
    type Error: BackendError + From<crate::Error>;

    fn dialect(&self) -> &Self::Dialect;

    /// Runs `plan` if the database still matches the catalog entries it
    /// was completed from ([`CatalogPlan::entries`]), and fails with
    /// [`ErrorKind::StaleCatalog`] otherwise. Rows are fetched as the
    /// returned solutions are read.
    fn execute(
        &self,
        plan: &CatalogPlan,
    ) -> impl Future<Output = Result<Self::Solutions, Self::Error>> + Send;

    /// The catalog entries of the logical tables `request` names. A table
    /// or column that does not exist is part of the snapshot, not an error.
    fn load_catalog(
        &self,
        request: &CatalogRequest,
    ) -> impl Future<Output = Result<CatalogSnapshot, Self::Error>> + Send;

    /// Whether the session is gone, e.g. after a server restart, so that a
    /// pool does not hand it out again.
    fn is_closed(&self) -> bool;
}

/// The solutions of one query, read one at a time.
pub trait SolutionStream: Send {
    type Error: BackendError;

    /// Projected variable names, without `?`.
    fn variables(&self) -> &[String];

    /// The next solution, or `None` when there are no more or the row
    /// limit is reached; [`is_truncated`](Self::is_truncated) tells which.
    fn try_next(&mut self) -> impl Future<Output = Result<Option<Solution>, Self::Error>> + Send;

    /// Whether the result has more solutions than the row limit. Known
    /// once the first solution has been read.
    fn is_truncated(&self) -> bool;
}

/// Opens [`Backend`] sessions, for front ends that need more than one,
/// such as the HTTP endpoint's connection pool.
pub trait Connector: Send + Sync {
    type Backend: Backend;

    /// A session that enforces `limits` on every query. The front end
    /// chooses the limits, so it can report them (the row limit in a
    /// truncation warning) without asking the backend.
    fn connect(
        &self,
        limits: Limits,
    ) -> impl Future<Output = Result<Self::Backend, <Self::Backend as Backend>::Error>> + Send;
}

/// An error of a backend, classified so that front ends choose exit codes
/// and HTTP statuses without knowing the backend.
pub trait BackendError: std::error::Error + Send + Sync + 'static {
    fn kind(&self) -> ErrorKind<'_>;
}

/// What a front end needs to know about a [`BackendError`].
#[derive(Debug, Clone, Copy)]
pub enum ErrorKind<'a> {
    /// The connection settings are invalid.
    Config,
    /// Connecting failed, or the database reported an error.
    Database,
    /// The database cancelled the query at
    /// [`Limits::statement_timeout`], which is given here.
    Timeout(Duration),
    /// The catalog no longer matches the snapshot the plan was completed
    /// from, so the plan was not run. Reading the catalog again and
    /// planning again can succeed.
    StaleCatalog,
    /// Rendering the plan for the actual column types, or turning a row
    /// into a solution, failed.
    Core(&'a crate::Error),
}

// SPDX-License-Identifier: Apache-2.0
//! SPARQL 1.1 Protocol endpoint (`/sparql`) plus health routes. Thin:
//! request handling, limits and streaming only.

pub mod config;
mod pool;
mod protocol;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use v12n_core::mapping::{Mapping, MappingIndex};
use v12n_core::results::{JSON_RESULTS_MEDIA_TYPE, JsonResultsWriter};
use v12n_core::solution::Solution;
use v12n_core::sql::SqlPlan;
use v12n_core::{sparql, unfold};
use v12n_postgres::Solutions;

pub use config::{Config, DatabaseConfig, ServerLimits};
use pool::{Lease, Pool};

/// Marks a response cut off at the row limit. Its value is the row limit,
/// which is also the number of solutions in the body. Virtuoso's
/// `X-SPARQL-MaxRows` is the prior art; RFC 6648 retires the `X-` prefix.
pub const TRUNCATED_HEADER: &str = "v12n-truncated";
/// Body bytes gathered before a chunk is handed to the connection.
const CHUNK_BYTES: usize = 16 * 1024;

/// Everything that stops the server from starting or running.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid or missing settings.
    #[error("config: {0}")]
    Config(String),
    /// The mapping file cannot be read or is not a valid R2RML mapping.
    #[error("mapping {path}: {message}")]
    Mapping { path: PathBuf, message: String },
    /// The startup connection to PostgreSQL failed.
    #[error(transparent)]
    Database(#[from] v12n_postgres::Error),
    /// Binding the listen address or serving connections failed.
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Loads the mapping, checks that the database is reachable, and serves
/// until SIGINT or SIGTERM. In-flight responses finish first.
pub async fn serve(config: Config) -> Result<(), Error> {
    let text = std::fs::read_to_string(&config.mapping).map_err(|e| Error::Mapping {
        path: config.mapping.clone(),
        message: e.to_string(),
    })?;
    let mapping = Mapping::from_turtle(&text).map_err(|e| Error::Mapping {
        path: config.mapping.clone(),
        message: e.to_string(),
    })?;
    let app = App::new(mapping, config.database, config.limits);
    app.pool.get().await?.release();

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(
        listen = %listener.local_addr()?,
        mapping = %config.mapping.display(),
        statement_timeout_ms = config.limits.query.statement_timeout.as_millis() as u64,
        row_limit = config.limits.query.row_limit,
        max_query_bytes = config.limits.max_query_bytes,
        max_concurrent_requests = config.limits.max_concurrent_requests,
        "serving SPARQL at /sparql"
    );
    axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}

/// Shared state of the endpoint.
pub struct App {
    mapping: Mapping,
    index: MappingIndex,
    pool: Arc<Pool>,
    permits: Arc<Semaphore>,
    limits: ServerLimits,
    next_query_id: AtomicU64,
}

impl App {
    /// Connections are opened on demand, at most one per concurrent
    /// request.
    pub fn new(mapping: Mapping, database: DatabaseConfig, limits: ServerLimits) -> Arc<Self> {
        let index = MappingIndex::new(&mapping);
        Arc::new(Self {
            mapping,
            index,
            pool: Pool::new(database, limits.query),
            permits: Arc::new(Semaphore::new(limits.max_concurrent_requests)),
            limits,
            next_query_id: AtomicU64::new(1),
        })
    }
}

/// `/sparql` for queries and `/healthz` for liveness. Other methods on
/// `/sparql` get 405 with an `Allow` header.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/sparql", get(sparql_query).post(sparql_query))
        .route("/healthz", get(|| async { "ok\n" }))
        .with_state(app)
}

/// An error response decided before the body starts.
#[derive(Debug)]
pub(crate) struct Failure {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
}

impl Failure {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }
}

impl IntoResponse for Failure {
    /// The SPARQL Protocol leaves error bodies to the implementation
    /// (§2.1.7); plain text keeps them readable with `curl`.
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("{}\n", self.message),
        )
            .into_response();
        if self.status == StatusCode::SERVICE_UNAVAILABLE && self.kind == "busy" {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

async fn sparql_query(
    State(app): State<Arc<App>>,
    method: Method,
    headers: HeaderMap,
    RawQuery(url_query): RawQuery,
    body: Body,
) -> Response {
    let query_id = app.next_query_id.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    match answer(&app, query_id, started, &method, &headers, url_query, body).await {
        Ok(response) => response,
        Err(failure) => {
            // Messages of database and data errors can quote stored
            // values, so only the kind is logged; the client gets the
            // message.
            let message = match failure.kind {
                "database" | "data" => "",
                _ => failure.message.as_str(),
            };
            tracing::warn!(
                query_id,
                status = failure.status.as_u16(),
                kind = failure.kind,
                total_ms = started.elapsed().as_millis() as u64,
                error = message,
                "refused"
            );
            failure.into_response()
        }
    }
}

async fn answer(
    app: &Arc<App>,
    query_id: u64,
    started: Instant,
    method: &Method,
    headers: &HeaderMap,
    url_query: Option<String>,
    body: Body,
) -> Result<Response, Failure> {
    if !protocol::accepts_json_results(headers) {
        return Err(Failure::new(
            StatusCode::NOT_ACCEPTABLE,
            "not_acceptable",
            format!("results are available as {JSON_RESULTS_MEDIA_TYPE} only"),
        ));
    }
    let query = protocol::read_query(
        method,
        headers,
        url_query.as_deref(),
        body,
        app.limits.max_query_bytes,
    )
    .await?;
    let plan = plan(app, &query)?;
    tracing::info!(query_id, sql = %plan.to_sql().sql, "unfolded");

    // Refusing at once instead of queueing keeps a burst from holding
    // client connections open with no bound.
    let permit = Arc::clone(&app.permits).try_acquire_owned().map_err(|_| {
        Failure::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            format!(
                "too many concurrent requests (limit {})",
                app.limits.max_concurrent_requests
            ),
        )
    })?;
    let lease = app.pool.get().await.map_err(database_failure)?;
    let sql_started = Instant::now();
    let mut solutions = lease
        .database()
        .execute(&plan)
        .await
        .map_err(database_failure)?;
    // The status line goes out before the body (RFC 9110 §15), so the
    // first row is awaited while an error can still become a status.
    let first = solutions.try_next().await.map_err(database_failure)?;
    let sql_ms = sql_started.elapsed().as_millis() as u64;

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(JSON_RESULTS_MEDIA_TYPE),
    );
    response_headers.insert(header::VARY, HeaderValue::from_static("accept"));
    if solutions.is_truncated() {
        response_headers.insert(
            HeaderName::from_static(TRUNCATED_HEADER),
            HeaderValue::from(app.limits.query.row_limit),
        );
    }
    let writer = JsonResultsWriter::start(Vec::with_capacity(CHUNK_BYTES), solutions.variables())
        .map_err(|e| {
        Failure::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    })?;
    let stream = ResultStream {
        writer: Some(writer),
        truncated: solutions.is_truncated(),
        solutions,
        next: first,
        lease: Some(lease),
        _permit: permit,
        query_id,
        started,
        sql_ms,
        rows: 0,
    };
    let body = futures_util::stream::unfold(stream, |mut stream| async move {
        stream.next_chunk().await.map(|chunk| (chunk, stream))
    });
    Ok((response_headers, Body::from_stream(body)).into_response())
}

fn plan(app: &App, query: &str) -> Result<SqlPlan, Failure> {
    sparql::parse(query)
        .and_then(|query| unfold::unfold(&query, &app.mapping, &app.index))
        .map_err(|e| {
            let (status, kind) = match e {
                v12n_core::Error::QuerySyntax(_) => (StatusCode::BAD_REQUEST, "query"),
                // A limit on the request, not a missing feature.
                v12n_core::Error::TooManyBranches { .. } => {
                    (StatusCode::BAD_REQUEST, "query_too_complex")
                }
                v12n_core::Error::UnsupportedFeature(_) => {
                    (StatusCode::NOT_IMPLEMENTED, "unsupported")
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
            };
            Failure::new(status, kind, e.to_string())
        })
}

fn database_failure(error: v12n_postgres::Error) -> Failure {
    use v12n_postgres::Error;
    let (status, kind) = match &error {
        // Before the first row, so the client may retry later.
        Error::Timeout(_) => (StatusCode::SERVICE_UNAVAILABLE, "timeout"),
        Error::Core(_) => (StatusCode::INTERNAL_SERVER_ERROR, "data"),
        Error::Config(_) | Error::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, "database"),
    };
    Failure::new(status, kind, error.to_string())
}

/// Turns solutions into body chunks, holding the request permit and the
/// connection until the last one is sent or the client goes away.
struct ResultStream {
    /// `None` once the closing bracket is written.
    writer: Option<JsonResultsWriter<Vec<u8>>>,
    solutions: Solutions,
    next: Option<Solution>,
    lease: Option<Lease>,
    _permit: OwnedSemaphorePermit,
    truncated: bool,
    query_id: u64,
    started: Instant,
    /// From sending the SQL to the first row, which with a row limit
    /// covers most of the execution (see `v12n_postgres`).
    sql_ms: u64,
    rows: u64,
}

impl ResultStream {
    /// The next chunk of at least [`CHUNK_BYTES`] bytes, except the last.
    ///
    /// An error here comes after the status line, so it cannot become an
    /// error status. Returning it makes the server abort the response
    /// (no terminating chunk in HTTP/1.1), and the unterminated JSON
    /// cannot pass for a complete result.
    async fn next_chunk(&mut self) -> Option<Result<Bytes, io::Error>> {
        let writer = self.writer.as_mut()?;
        while let Some(solution) = self.next.take() {
            if let Err(e) = writer.write(&solution) {
                return Some(Err(self.abort("internal", e.to_string())));
            }
            self.rows += 1;
            match self.solutions.try_next().await {
                Ok(next) => self.next = next,
                Err(e) => {
                    // Only the timeout message is free of stored values.
                    let (kind, message) = match e {
                        v12n_postgres::Error::Timeout(_) => ("timeout", e.to_string()),
                        v12n_postgres::Error::Core(_) => ("data", String::new()),
                        _ => ("database", String::new()),
                    };
                    return Some(Err(self.abort(kind, message)));
                }
            }
            if writer.get_mut().len() >= CHUNK_BYTES {
                return Some(Ok(Bytes::from(std::mem::take(writer.get_mut()))));
            }
        }
        let tail = match self.writer.take()?.finish() {
            Ok(tail) => tail,
            Err(e) => return Some(Err(self.abort("internal", e.to_string()))),
        };
        if let Some(lease) = self.lease.take() {
            lease.release();
        }
        tracing::info!(
            query_id = self.query_id,
            status = 200,
            row_count = self.rows,
            truncated = self.truncated,
            sql_ms = self.sql_ms,
            total_ms = self.started.elapsed().as_millis() as u64,
            "answered"
        );
        Some(Ok(Bytes::from(tail)))
    }

    fn abort(&mut self, kind: &'static str, message: String) -> io::Error {
        self.writer = None;
        self.lease = None;
        tracing::error!(
            query_id = self.query_id,
            kind,
            row_count = self.rows,
            total_ms = self.started.elapsed().as_millis() as u64,
            error = message,
            "response aborted mid-stream"
        );
        io::Error::other(format!(
            "query {} failed mid-response: {kind}",
            self.query_id
        ))
    }
}

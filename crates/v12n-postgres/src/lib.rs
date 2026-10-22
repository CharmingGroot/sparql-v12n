// SPDX-License-Identifier: Apache-2.0
//! Runs SQL produced by `v12n-core` on PostgreSQL and streams rows back as
//! SPARQL solutions.

mod decode;
mod error;

use std::pin::Pin;
use std::time::Duration;

use futures_util::TryStreamExt;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Config, NoTls, RowStream, Statement};
use v12n_core::solution::{Solution, SolutionBuilder, SqlValue};
use v12n_core::sql::{SqlPlan, quote_identifier};

use decode::{Decoder, Raw, TextParam, can_match};
pub use error::Error;

/// Default for [`Limits::statement_timeout`].
pub const DEFAULT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default for [`Limits::row_limit`].
pub const DEFAULT_ROW_LIMIT: u64 = 10_000;
/// Applied when the connection string sets no `connect_timeout`.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Resource limits for every query run on a [`Database`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// PostgreSQL `statement_timeout`; zero disables it.
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

/// A connection that runs [`SqlPlan`]s.
pub struct Database {
    client: Client,
    limits: Limits,
}

impl Database {
    /// Connects with a libpq-style connection string or URL and sets up the
    /// session: the statement timeout, and read-only transactions so that
    /// an `rr:sqlQuery` cannot change data even through a function call.
    ///
    /// Must be called inside a Tokio runtime, which drives the connection.
    pub async fn connect(url: &str, limits: Limits) -> Result<Self, Error> {
        let mut config: Config = url.parse().map_err(Error::config)?;
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(DEFAULT_CONNECT_TIMEOUT);
        }
        let timeout = limits.statement_timeout;
        let (client, connection) = config
            .connect(NoTls)
            .await
            .map_err(|e| Error::from_postgres(e, timeout))?;
        // Connection failures also surface as errors on the client's next
        // request, so the task's own result adds nothing.
        tokio::spawn(connection);
        let timeout_ms = format!("{}ms", timeout.as_millis());
        client
            .execute(
                "SELECT set_config('statement_timeout', $1, false), \
                 set_config('default_transaction_read_only', 'on', false)",
                &[&timeout_ms],
            )
            .await
            .map_err(|e| Error::from_postgres(e, timeout))?;
        Ok(Self { client, limits })
    }

    /// Runs a plan. Rows are fetched as the returned [`Solutions`] are read.
    pub async fn execute(&self, plan: &SqlPlan) -> Result<Solutions, Error> {
        let timeout = self.limits.statement_timeout;
        let pg = |e| Error::from_postgres(e, timeout);
        let rendered = plan.to_sql();
        // One row past the limit tells a cut-off result from one that
        // happens to have exactly `row_limit` rows.
        let fetch = (self.limits.row_limit > 0)
            .then(|| i64::try_from(self.limits.row_limit.saturating_add(1)).unwrap_or(i64::MAX));
        let limit_clause = match fetch {
            Some(_) => format!("\nLIMIT ${}", rendered.params.len() + 1),
            None => String::new(),
        };

        let mut statement = self
            .client
            .prepare(&format!("{}{limit_clause}", rendered.sql))
            .await
            .map_err(pg)?;
        let decoders: Vec<Option<Decoder>> = statement
            .columns()
            .iter()
            .map(|c| Decoder::for_type(c.type_()))
            .collect();
        if decoders.iter().any(Option::is_none) && decoders.len() == plan.columns.len() {
            let sql = cast_to_text(&rendered.sql, &plan.columns, &decoders);
            statement = self
                .client
                .prepare(&format!("{sql}{limit_clause}"))
                .await
                .map_err(pg)?;
        }
        let decoders = statement
            .columns()
            .iter()
            .map(|c| Decoder::for_type(c.type_()).unwrap_or(Decoder::Text))
            .collect();

        let mut solutions = Solutions {
            rows: None,
            builder: SolutionBuilder::new(plan),
            decoders,
            row_limit: self.limits.row_limit,
            returned: 0,
            truncated: false,
            timeout,
        };
        let types = statement.params();
        if rendered
            .params
            .iter()
            .zip(types)
            .any(|(value, ty)| !can_match(ty, value))
        {
            return Ok(solutions);
        }
        solutions.rows = Some(Box::pin(
            self.query(&statement, &rendered.params, fetch)
                .await
                .map_err(pg)?,
        ));
        Ok(solutions)
    }

    async fn query(
        &self,
        statement: &Statement,
        params: &[String],
        fetch: Option<i64>,
    ) -> Result<RowStream, tokio_postgres::Error> {
        let texts: Vec<TextParam<'_>> = params.iter().map(|p| TextParam(p)).collect();
        let mut bound: Vec<&(dyn ToSql + Sync)> =
            texts.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        if let Some(fetch) = &fetch {
            bound.push(fetch);
        }
        self.client.query_raw(statement, bound).await
    }
}

/// R2RML §10.2: a value whose type has no natural datatype becomes a plain
/// literal of the value cast to a string. Casting in SQL avoids decoding
/// every binary format PostgreSQL has.
fn cast_to_text(sql: &str, columns: &[String], decoders: &[Option<Decoder>]) -> String {
    let select: Vec<String> = columns
        .iter()
        .zip(decoders)
        .map(|(column, decoder)| {
            let column = format!("q.{}", quote_identifier(column));
            match decoder {
                Some(_) => column,
                None => format!("{column}::text"),
            }
        })
        .collect();
    format!("SELECT {}\nFROM (\n{sql}\n) q", select.join(", "))
}

/// The solutions of one query, read one row at a time.
pub struct Solutions {
    /// `None` once exhausted, or when no row can match.
    rows: Option<Pin<Box<RowStream>>>,
    builder: SolutionBuilder,
    decoders: Vec<Decoder>,
    row_limit: u64,
    returned: u64,
    truncated: bool,
    timeout: Duration,
}

impl Solutions {
    /// Projected variable names, without `?`.
    pub fn variables(&self) -> &[String] {
        self.builder.variables()
    }

    /// The next solution, or `None` when there are no more or the row
    /// limit is reached; [`is_truncated`](Self::is_truncated) tells which.
    pub async fn try_next(&mut self) -> Result<Option<Solution>, Error> {
        let Some(rows) = self.rows.as_mut() else {
            return Ok(None);
        };
        let row = match rows.try_next().await {
            Ok(Some(row)) => row,
            Ok(None) => {
                self.rows = None;
                return Ok(None);
            }
            Err(e) => {
                self.rows = None;
                return Err(Error::from_postgres(e, self.timeout));
            }
        };
        if self.row_limit > 0 && self.returned == self.row_limit {
            self.truncated = true;
            self.rows = None;
            return Ok(None);
        }
        self.returned += 1;
        let values = self
            .decoders
            .iter()
            .enumerate()
            .map(|(i, decoder)| {
                let Raw(raw) = row.try_get(i).map_err(|e| Error::Database(e.to_string()))?;
                decoder.decode(raw).map_err(|e| {
                    Error::Database(format!(
                        "cannot read column {}: {e}",
                        row.columns()[i].name()
                    ))
                })
            })
            .collect::<Result<Vec<SqlValue>, Error>>()?;
        Ok(Some(self.builder.build(&values)?))
    }

    /// Whether the result had more rows than the row limit.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

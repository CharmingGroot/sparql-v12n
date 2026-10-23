// SPDX-License-Identifier: Apache-2.0
//! Runs SQL produced by `v12n-core` on PostgreSQL and streams rows back as
//! SPARQL solutions.

mod bind;
mod decode;
mod error;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::time::Duration;

use futures_util::TryStreamExt;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Config, NoTls, RowStream, Statement};
use v12n_core::mapping::LogicalTable;
use v12n_core::solution::{Solution, SolutionBuilder, SqlValue};
use v12n_core::sql::{
    ColumnTypes, RenderOptions, SqlPlan, TableKeys, column_name_probe, column_type_probe,
    quote_identifier, quote_table_name,
};

use bind::{Bound, bind};
use decode::{Decoder, Raw, column_type};
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
        let config: Config = url.parse().map_err(Error::config)?;
        Self::connect_config(config, limits).await
    }

    /// Like [`connect`](Self::connect), with a password kept out of the
    /// URL, for example one read from a separate environment variable.
    pub async fn connect_with_password(
        url: &str,
        password: &str,
        limits: Limits,
    ) -> Result<Self, Error> {
        let mut config: Config = url.parse().map_err(Error::config)?;
        config.password(password);
        Self::connect_config(config, limits).await
    }

    async fn connect_config(mut config: Config, limits: Limits) -> Result<Self, Error> {
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

    /// Whether the connection is gone, e.g. after a server restart.
    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// Runs a plan. Rows are fetched as the returned [`Solutions`] are read.
    pub async fn execute(&self, plan: &SqlPlan) -> Result<Solutions, Error> {
        let timeout = self.limits.statement_timeout;
        let pg = |e| Error::from_postgres(e, timeout);
        let plan = self.resolve_columns(plan).await.map_err(pg)?;
        let column_types = self.column_types(&plan).await.map_err(pg)?;
        let plan = match &column_types {
            Some(types) if plan.branches.len() > 1 => Cow::Owned(plan.with_union_types(types)?),
            _ => plan,
        };
        let plan = plan.as_ref();
        let keys = self.load_table_keys(plan).await.map_err(pg)?;
        let options = RenderOptions {
            text_params: &[],
            column_types: column_types.as_ref(),
            keys: keys.as_ref(),
        };
        let rendered = plan.to_sql_with(&options)?;
        // One row past the limit tells a cut-off result from one that
        // happens to have exactly `row_limit` rows.
        let fetch = (self.limits.row_limit > 0)
            .then(|| i64::try_from(self.limits.row_limit.saturating_add(1)).unwrap_or(i64::MAX));
        let limited = |sql: &str| match fetch {
            Some(_) => count_rows(sql, rendered.params.len() + 1),
            None => sql.to_owned(),
        };

        let mut statement = self
            .client
            .prepare(&limited(&rendered.sql))
            .await
            .map_err(pg)?;
        let data_columns = statement.columns().len() - usize::from(fetch.is_some());
        let decoders: Vec<Option<Decoder>> = statement.columns()[..data_columns]
            .iter()
            .map(|c| Decoder::for_type(c.type_()))
            .collect();
        // Values of types without a decoder become terms through their text
        // output (see `cast_to_text`), so a constant compared with one is
        // matched against that text instead of parsed as the type.
        let text_params: Vec<usize> = statement.params()[..rendered.params.len()]
            .iter()
            .enumerate()
            .filter(|(_, ty)| Decoder::for_type(ty).is_none())
            .map(|(i, _)| i)
            .collect();
        let cast_columns =
            decoders.iter().any(Option::is_none) && decoders.len() == plan.columns.len();
        let rendered = if cast_columns || !text_params.is_empty() {
            let rendered = plan.to_sql_with(&RenderOptions {
                text_params: &text_params,
                ..options
            })?;
            let sql = if cast_columns {
                cast_to_text(&rendered.sql, &plan.columns, &decoders)
            } else {
                rendered.sql.clone()
            };
            statement = self.client.prepare(&limited(&sql)).await.map_err(pg)?;
            rendered
        } else {
            rendered
        };
        let decoders = statement.columns()[..data_columns]
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
        let bound: Vec<Bound> = rendered
            .params
            .iter()
            .zip(&rendered.param_datatypes)
            .zip(statement.params())
            .map(|((value, datatype), ty)| bind(ty, value, datatype.as_deref()))
            .collect();
        // Skipping the round trip when no branch can produce a row also
        // covers the common single-branch case. An alternative of an `IN`
        // list that cannot match only drops itself, and a constant of an
        // `OPTIONAL` part or a disjunction only fails its own condition.
        let mut empty_branches: HashSet<usize> = HashSet::new();
        let mut alternatives: HashMap<usize, (usize, bool)> = HashMap::new();
        for (((b, &branch), alternative), _) in bound
            .iter()
            .zip(&rendered.param_branches)
            .zip(&rendered.param_alternatives)
            .zip(&rendered.param_required)
            .filter(|(_, required)| **required)
        {
            match alternative {
                Some(group) => {
                    alternatives.entry(*group).or_insert((branch, false)).1 |= !b.is_no_match()
                }
                None if b.is_no_match() => {
                    empty_branches.insert(branch);
                }
                None => {}
            }
        }
        empty_branches.extend(
            alternatives
                .values()
                .filter(|(_, can_match)| !can_match)
                .map(|&(branch, _)| branch),
        );
        if !plan.branches.is_empty() && empty_branches.len() == plan.branches.len() {
            return Ok(solutions);
        }
        solutions.rows = Some(Box::pin(
            self.query(&statement, &bound, fetch).await.map_err(pg)?,
        ));
        Ok(solutions)
    }

    /// Unique keys of the tables that can show the plan has no duplicate
    /// solutions (`SqlPlan::key_tables`), read from the catalog on every
    /// query so that a dropped key is never trusted. `None` when keys
    /// cannot help.
    ///
    /// Only keys that hold for every row at any time count: unique
    /// indexes that are valid, not deferrable, not partial, and on plain
    /// columns. A view or a table that does not exist has none, and so
    /// does a table with inheritance children, whose rows the scan reads
    /// but its indexes do not cover (a partitioned table's keys cover its
    /// partitions).
    pub async fn table_keys(&self, plan: &SqlPlan) -> Result<Option<TableKeys>, Error> {
        let timeout = self.limits.statement_timeout;
        self.load_table_keys(plan)
            .await
            .map_err(|e| Error::from_postgres(e, timeout))
    }

    async fn load_table_keys(
        &self,
        plan: &SqlPlan,
    ) -> Result<Option<TableKeys>, tokio_postgres::Error> {
        let tables = plan.key_tables();
        if tables.is_empty() {
            return Ok(None);
        }
        let names: Vec<String> = tables
            .iter()
            .map(|t| match t {
                LogicalTable::TableName(name) => quote_table_name(name),
                LogicalTable::SqlQuery(_) => unreachable!("key_tables lists tables only"),
            })
            .collect();
        let rows = self
            .client
            .query(
                "SELECT t.n::int4, array_agg(a.attname::text ORDER BY c.ord)
                 FROM unnest($1::text[]) WITH ORDINALITY AS t(name, n)
                 JOIN pg_index i ON i.indrelid = to_regclass(t.name)
                 JOIN pg_class r ON r.oid = i.indrelid
                 CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS c(attnum, ord)
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = c.attnum
                 WHERE i.indisunique AND i.indimmediate AND i.indisvalid
                   AND i.indpred IS NULL AND i.indexprs IS NULL AND c.ord <= i.indnkeyatts
                   AND (r.relkind = 'p'
                     OR NOT EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhparent = r.oid))
                 GROUP BY t.n, i.indexrelid",
                &[&names],
            )
            .await?;
        let mut keys = TableKeys::new();
        for row in rows {
            let position: i32 = row.get(0);
            let columns: Vec<String> = row.get(1);
            let table = usize::try_from(position - 1).expect("ordinality starts at 1");
            keys.entry(tables[table].clone()).or_default().push(columns);
        }
        Ok(Some(keys))
    }

    /// The plan with every column name that PostgreSQL may fold
    /// (`folded_identifier`) replaced by the name PostgreSQL resolves it
    /// to, read from the result columns of a prepared probe statement.
    /// The database applies its own folding rules, which depend on the
    /// encoding, and an `rr:sqlQuery` names its columns as PostgreSQL
    /// does (`AS StudentId` is `studentid`, `count(*)` is `count`).
    async fn resolve_columns<'p>(
        &self,
        plan: &'p SqlPlan,
    ) -> Result<Cow<'p, SqlPlan>, tokio_postgres::Error> {
        let columns = plan.folded_columns();
        if columns.is_empty() {
            return Ok(Cow::Borrowed(plan));
        }
        let probe = self.client.prepare(&column_name_probe(&columns)).await?;
        let resolved: HashMap<_, _> = columns
            .into_iter()
            .zip(probe.columns())
            .map(|(column, c)| (column, c.name().to_owned()))
            .collect();
        Ok(Cow::Owned(plan.with_resolved_columns(&resolved)))
    }

    /// Types of the columns the plan compares as RDF terms or combines
    /// with `UNION`, read by preparing a statement that selects them.
    /// `None` when there are none.
    async fn column_types(
        &self,
        plan: &SqlPlan,
    ) -> Result<Option<ColumnTypes>, tokio_postgres::Error> {
        let mut columns = plan.term_columns();
        for column in plan.union_columns() {
            if !columns.contains(&column) {
                columns.push(column);
            }
        }
        if columns.is_empty() {
            return Ok(None);
        }
        let probe = self.client.prepare(&column_type_probe(&columns)).await?;
        Ok(Some(
            columns
                .into_iter()
                .zip(probe.columns())
                .map(|(column, c)| (column, column_type(c.type_())))
                .collect(),
        ))
    }

    async fn query(
        &self,
        statement: &Statement,
        params: &[Bound],
        fetch: Option<i64>,
    ) -> Result<RowStream, tokio_postgres::Error> {
        let mut bound: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        if let Some(fetch) = &fetch {
            bound.push(fetch);
        }
        self.client.query_raw(statement, bound).await
    }
}

/// Wraps `sql` so that every row also carries the number of rows, at most
/// `$limit`, in an extra last column. The first row then tells whether
/// the result exceeds the row limit, before any solution is handed out,
/// so an HTTP response can mark the truncation in a header. PostgreSQL
/// buffers the limited rows for the window; the client still streams.
/// An empty `OVER ()` adds no sort, so the order of `sql` is kept.
fn count_rows(sql: &str, limit_param: usize) -> String {
    format!("SELECT q.*, count(*) OVER ()\nFROM (\n{sql}\nLIMIT ${limit_param}\n) q")
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
        if self.row_limit > 0 {
            if self.returned == 0 {
                let total: i64 = row
                    .try_get(self.decoders.len())
                    .map_err(|e| Error::Database(e.to_string()))?;
                self.truncated = u64::try_from(total).unwrap_or(0) > self.row_limit;
            }
            if self.returned == self.row_limit {
                self.rows = None;
                return Ok(None);
            }
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

    /// Whether the result has more rows than the row limit. Known once
    /// the first solution has been read.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

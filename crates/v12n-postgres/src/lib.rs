// SPDX-License-Identifier: Apache-2.0
//! Runs SQL produced by `v12n-core` on PostgreSQL and streams rows back as
//! SPARQL solutions: the PostgreSQL adapter of the core's
//! [`Backend`] port.

mod bind;
mod catalog;
mod connector;
mod decode;
mod error;
mod tls;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures_util::TryStreamExt;
use futures_util::future::{join, join3};
use tls::Tls;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Config, RowStream, Statement};
use v12n_core::Limits;
use v12n_core::backend::{Backend, SolutionStream};
use v12n_core::catalog::{CatalogPlan, CatalogRequest, CatalogSnapshot, PlanKeys};
use v12n_core::dialect::PostgresDialect;
use v12n_core::optimize;
use v12n_core::solution::{Solution, SolutionBuilder, SqlValue};
use v12n_core::sql::{
    RenderOptions, RenderedSql, SqlPlan, TableKeys, TermKey, TermNormal, quote_identifier,
};

use bind::{Bound, bind, term_key, term_normal};
pub use connector::PostgresConnector;
use decode::{Decoder, Raw};
pub use error::Error;

/// Applied when the connection string sets no `connect_timeout`.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Most statements whose retyped SQL a connection remembers
/// ([`PostgresBackend::retyped`]).
const RETYPED_STATEMENTS: usize = 64;

/// The unique keys of the tables named in `$1` that hold for every row at
/// any time: unique indexes that are valid, not deferrable, not partial
/// and on plain columns. A table with inheritance children has none, since
/// a scan reads rows its indexes do not cover; a partitioned table's keys
/// cover its partitions. The third column tells whether the index decides
/// equality as SQL `=` does (`UniqueKey::exact_index`).
const KEYS_SQL: &str = "SELECT t.n::int4, array_agg(a.attname::text ORDER BY c.ord),
   bool_and(a.attnotnull AND c.coll = a.attcollation AND o.opcdefault),
   array_agg(a.atttypid ORDER BY c.ord)
 FROM unnest($1::text[]) WITH ORDINALITY AS t(name, n)
 JOIN pg_index i ON i.indrelid = to_regclass(t.name)
 JOIN pg_class r ON r.oid = i.indrelid
 CROSS JOIN LATERAL unnest(i.indkey::int2[], i.indcollation::oid[],
   i.indclass::oid[]) WITH ORDINALITY AS c(attnum, coll, opclass, ord)
 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = c.attnum
 JOIN pg_opclass o ON o.oid = c.opclass
 WHERE i.indisunique AND i.indimmediate AND i.indisvalid
   AND i.indpred IS NULL AND i.indexprs IS NULL AND c.ord <= i.indnkeyatts
   AND (r.relkind = 'p'
     OR NOT EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhparent = r.oid))
 GROUP BY t.n, i.indexrelid";

/// A connection that runs [`CatalogPlan`]s.
pub struct PostgresBackend {
    client: Client,
    limits: Limits,
    /// Whether the server encoding is UTF-8, where `COLLATE "C"` orders
    /// strings by code point.
    utf8: bool,
    catalog: catalog::CatalogStatements,
    /// For statements whose parameter or result column types made
    /// [`run`](Self::run) render and prepare them again, the SQL it
    /// prepared instead, by the SQL it rendered first. Only a guess of what
    /// to prepare: the next query with that SQL prepares the guess in the
    /// same round trip as the first statement, and uses it only if it is
    /// exactly the statement the types call for (Discussion #87).
    retyped: Mutex<HashMap<String, String>>,
}

impl PostgresBackend {
    /// Connects with a libpq-style connection string or URL and sets up the
    /// session: the statement timeout, and read-only transactions so that
    /// an `rr:sqlQuery` cannot change data even through a function call.
    ///
    /// TLS follows libpq's `sslmode` (`prefer` when unset) and
    /// `sslrootcert`. Without `sslrootcert`, `verify-ca` and `verify-full`
    /// check the certificate against the system's root certificates.
    ///
    /// Must be called inside a Tokio runtime, which drives the connection.
    pub async fn connect(url: &str, limits: Limits) -> Result<Self, Error> {
        let (config, tls) = tls::parse(url)?;
        Self::connect_config(config, &tls, limits).await
    }

    /// Like [`connect`](Self::connect), with a password kept out of the
    /// URL, for example one read from a separate environment variable.
    pub async fn connect_with_password(
        url: &str,
        password: &str,
        limits: Limits,
    ) -> Result<Self, Error> {
        let (mut config, tls) = tls::parse(url)?;
        config.password(password);
        Self::connect_config(config, &tls, limits).await
    }

    async fn connect_config(mut config: Config, tls: &Tls, limits: Limits) -> Result<Self, Error> {
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(DEFAULT_CONNECT_TIMEOUT);
        }
        let timeout = limits.statement_timeout;
        let client = tls.connect(&config, timeout).await?;
        let timeout_ms = format!("{}ms", timeout.as_millis());
        let (row, catalog) = join(
            client.query_one(
                "SELECT set_config('statement_timeout', $1, false), \
                 set_config('default_transaction_read_only', 'on', false), \
                 current_setting('server_encoding')",
                &[&timeout_ms],
            ),
            catalog::CatalogStatements::prepare(&client),
        )
        .await;
        let row = row.map_err(|e| Error::from_postgres(e, timeout))?;
        let catalog = catalog.map_err(|e| Error::from_postgres(e, timeout))?;
        let encoding: String = row.get(2);
        Ok(Self {
            client,
            limits,
            utf8: encoding == "UTF8",
            catalog,
            retyped: Mutex::new(HashMap::new()),
        })
    }

    /// Runs a plan for [`Backend::execute`]. The check that the database
    /// still matches the catalog entries of the plan goes out with the
    /// preparation of the statement, in one pipelined round trip.
    async fn run(&self, ready: &CatalogPlan) -> Result<Solutions, Error> {
        let pg = |e| Error::from_postgres(e, self.limits.statement_timeout);
        let plan = ready.plan();
        let options = RenderOptions {
            non_utf8: !self.utf8,
            ..ready.render_options()
        };
        let rendered = plan.to_sql_with(&PostgresDialect, &options)?;
        let fetch = row_fetch(self.limits.row_limit, plan.limit);
        let sql = statement_sql(plan, &rendered, None, fetch.is_some());
        let guess = self
            .retyped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&sql)
            .cloned();
        let (current, statement, guessed) = join3(
            catalog::is_current(&self.client, &self.catalog, ready),
            self.client.prepare(&sql),
            self.prepare_if(guess.as_deref()),
        )
        .await;
        // A statement prepared on a changed catalog can fail because of
        // the change, so the check is reported first.
        if !current.map_err(pg)? {
            return Err(Error::StaleCatalog);
        }
        let statement = statement.map_err(pg)?;
        let (statement, rendered) = match retyped(plan, options, &statement, &rendered, fetch)? {
            None => (statement, rendered),
            Some((retyped_sql, retyped_rendered)) => {
                let statement = match guessed.and_then(Result::ok) {
                    Some(guessed) if guess.as_ref() == Some(&retyped_sql) => guessed,
                    _ => self.client.prepare(&retyped_sql).await.map_err(pg)?,
                };
                self.remember_retyped(sql, retyped_sql);
                (statement, retyped_rendered)
            }
        };
        self.start(plan, &statement, &rendered, fetch).await
    }

    async fn prepare_if(
        &self,
        sql: Option<&str>,
    ) -> Option<Result<Statement, tokio_postgres::Error>> {
        match sql {
            Some(sql) => Some(self.client.prepare(sql).await),
            None => None,
        }
    }

    fn remember_retyped(&self, sql: String, retyped_sql: String) {
        let mut retyped = self.retyped.lock().unwrap_or_else(PoisonError::into_inner);
        if retyped.len() >= RETYPED_STATEMENTS {
            retyped.clear();
        }
        retyped.insert(sql, retyped_sql);
    }

    /// Binds the constants and starts the statement, or returns no
    /// solutions without a round trip when no branch can produce a row.
    async fn start(
        &self,
        plan: &SqlPlan,
        statement: &Statement,
        rendered: &RenderedSql,
        fetch: Option<i64>,
    ) -> Result<Solutions, Error> {
        let timeout = self.limits.statement_timeout;
        // The projected columns, then the sort keys, then the row count.
        // A query that selects neither selects a constant in their place.
        let data_columns = plan.columns.len();
        let placeholder = usize::from(data_columns == 0 && rendered.order_by.is_empty());
        let count_column = fetch.map(|_| data_columns + rendered.order_by.len() + placeholder);
        let decoders = statement.columns()[..data_columns]
            .iter()
            .map(|c| Decoder::for_type(c.type_()).unwrap_or(Decoder::Text))
            .collect();
        let mut solutions = Solutions {
            rows: None,
            builder: SolutionBuilder::new(plan),
            decoders,
            row_limit: self.limits.row_limit,
            count_column,
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
        if !plan.branches.is_empty() && no_branch_can_match(plan, rendered, &bound) {
            return Ok(solutions);
        }
        let rows = self
            .query(statement, &bound, fetch)
            .await
            .map_err(|e| Error::from_postgres(e, timeout))?;
        solutions.rows = Some(Box::pin(rows));
        Ok(solutions)
    }

    /// The unique keys that rendering uses for `plan`, read from the
    /// catalog now; `None` when keys cannot help. See
    /// [`CatalogSnapshot::plan_keys`].
    pub async fn table_keys(&self, plan: &SqlPlan) -> Result<Option<TableKeys>, Error> {
        Ok(self.plan_keys(plan).await?.map(|k| k.unique))
    }

    /// The keys that merge two scans of a table that a branch of `plan`
    /// scans more than once; `None` when no table is scanned twice.
    pub async fn merge_keys(&self, plan: &SqlPlan) -> Result<Option<TableKeys>, Error> {
        let keys = self.plan_keys(plan).await?;
        Ok(keys
            .filter(|_| !optimize::self_join_tables(plan).is_empty())
            .map(|k| k.mergeable))
    }

    async fn plan_keys(&self, plan: &SqlPlan) -> Result<Option<PlanKeys>, Error> {
        let snapshot = self.load_catalog(&CatalogRequest::for_plan(plan)).await?;
        Ok(snapshot.plan_keys(plan))
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

impl Backend for PostgresBackend {
    type Dialect = PostgresDialect;
    type Solutions = Solutions;
    type Error = Error;

    fn dialect(&self) -> &PostgresDialect {
        &PostgresDialect
    }

    fn execute(&self, plan: &CatalogPlan) -> impl Future<Output = Result<Solutions, Error>> + Send {
        self.run(plan)
    }

    async fn load_catalog(&self, request: &CatalogRequest) -> Result<CatalogSnapshot, Error> {
        catalog::load(&self.client, &self.catalog, request)
            .await
            .map_err(|e| Error::from_postgres(e, self.limits.statement_timeout))
    }

    fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}

/// The SQL and rendering for the types PostgreSQL gave the result columns
/// and the parameters of `statement`, when they differ from the first
/// rendering: a column without a decoder is cast to text, and a constant
/// that does not compare as its value compares as its term. `None` when
/// the first rendering stands.
fn retyped(
    plan: &SqlPlan,
    options: RenderOptions<'_>,
    statement: &Statement,
    rendered: &RenderedSql,
    fetch: Option<i64>,
) -> Result<Option<(String, RenderedSql)>, v12n_core::Error> {
    let decoders: Vec<Option<Decoder>> = statement.columns()[..plan.columns.len()]
        .iter()
        .map(|c| Decoder::for_type(c.type_()))
        .collect();
    let params = &statement.params()[..rendered.params.len()];
    // Values of types without a decoder become terms through their text
    // output (see `cast_to_text`), so a constant compared with one is
    // matched against that text instead of parsed as the type.
    let text_params: Vec<usize> = params
        .iter()
        .enumerate()
        .filter(|(_, ty)| Decoder::for_type(ty).is_none())
        .map(|(i, _)| i)
        .collect();
    // `=` finds a zero constant equal to `-0` and a `bpchar` constant
    // equal to values with more padding, which are other terms.
    let keyed_params: Vec<(usize, TermKey)> = params
        .iter()
        .zip(&rendered.params)
        .enumerate()
        .filter_map(|(i, (ty, value))| term_key(ty, value).map(|key| (i, key)))
        .collect();
    // `=` finds a midnight constant different from `24:00:00`, the same term.
    let normal_params: Vec<(usize, TermNormal)> = params
        .iter()
        .zip(&rendered.params)
        .enumerate()
        .filter_map(|(i, (ty, value))| term_normal(ty, value).map(|normal| (i, normal)))
        .collect();
    let cast_columns = decoders.iter().any(Option::is_none);
    if !cast_columns
        && text_params.is_empty()
        && keyed_params.is_empty()
        && normal_params.is_empty()
    {
        return Ok(None);
    }
    let rendered = plan.to_sql_with(
        &PostgresDialect,
        &RenderOptions {
            text_params: &text_params,
            keyed_params: &keyed_params,
            normal_params: &normal_params,
            ..options
        },
    )?;
    let cast = cast_columns.then_some(decoders.as_slice());
    let sql = statement_sql(plan, &rendered, cast, fetch.is_some());
    Ok(Some((sql, rendered)))
}

/// Whether no branch of `plan` can produce a row because a required
/// constant cannot match its column. An alternative of an `IN` list that
/// cannot match only drops itself, and a constant of an `OPTIONAL` part or
/// a disjunction only fails its own condition.
fn no_branch_can_match(plan: &SqlPlan, rendered: &RenderedSql, bound: &[Bound]) -> bool {
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
    empty_branches.len() == plan.branches.len()
}

/// How many rows the statement reads when the row limit can cut the
/// result: one past `row_limit`, which tells a cut-off result from one
/// with exactly `row_limit` rows (Discussion #22). `None` when nothing can
/// be cut: without a row limit, or when the query's own `LIMIT` is at or
/// below it, since Slice keeps at most `LIMIT` solutions (SPARQL 1.1
/// §18.5) and each row is one solution. The statement is then the plan's
/// own SQL, which ends in the plan's `ORDER BY`, `LIMIT` and `OFFSET`.
fn row_fetch(row_limit: u64, query_limit: Option<usize>) -> Option<i64> {
    let fits = query_limit.is_some_and(|limit| u64::try_from(limit).is_ok_and(|l| l <= row_limit));
    (row_limit > 0 && !fits).then(|| i64::try_from(row_limit.saturating_add(1)).unwrap_or(i64::MAX))
}

/// The statement that runs `rendered`: with the columns that `decoders`
/// has no decoder for cast to text, and with the row count when the
/// results are `limited` (then `$n` after the plan's parameters is the
/// row limit plus one, see [`row_fetch`]).
fn statement_sql(
    plan: &SqlPlan,
    rendered: &RenderedSql,
    decoders: Option<&[Option<Decoder>]>,
    limited: bool,
) -> String {
    let sql = match decoders {
        Some(decoders) => Cow::Owned(cast_to_text(rendered, &plan.columns, decoders)),
        None => Cow::Borrowed(rendered.sql.as_str()),
    };
    if !limited {
        return sql.into_owned();
    }
    let sliced = plan.limit.is_some() || plan.offset > 0;
    count_rows(&sql, rendered.params.len() + 1, sliced, |table| {
        rendered.order_by_clause(&PostgresDialect, table)
    })
}

/// Wraps `sql` so that every row also carries the number of rows, at most
/// `$limit`, in an extra last column. The first row then tells whether
/// the result exceeds the row limit, before any solution is handed out,
/// so an HTTP response can mark the truncation in a header. PostgreSQL
/// buffers the limited rows for the window; the client still streams.
/// An empty `OVER ()` adds no sort, so the order of `sql` is kept.
///
/// Only used when the result can exceed the row limit ([`row_fetch`]).
/// When `sql` has its own `LIMIT` or `OFFSET` (`sliced`), or its own
/// `ORDER BY`, it is read as a subquery, since one `SELECT` takes only
/// one `LIMIT`.
///
/// SQL keeps no order through a subquery, so with `ORDER BY` both the
/// limited subquery and the outermost query sort again by the sort keys
/// `sql` selects (`order_by` writes the clause for a table alias). Their
/// rows are already in that order, so this costs little, and the order of
/// the rows the client reads is the outermost query's.
fn count_rows(
    sql: &str,
    limit_param: usize,
    sliced: bool,
    order_by: impl Fn(&str) -> String,
) -> String {
    let inner = if sliced || !order_by("l").is_empty() {
        format!("SELECT *\nFROM (\n{sql}\n) l{}", order_by("l"))
    } else {
        sql.to_owned()
    };
    format!(
        "SELECT q.*, count(*) OVER ()\nFROM (\n{inner}\nLIMIT ${limit_param}\n) q{}",
        order_by("q")
    )
}

/// R2RML §10.2: a value whose type has no natural datatype becomes a plain
/// literal of the value cast to a string. Casting in SQL avoids decoding
/// every binary format PostgreSQL has. The sort keys pass through, and the
/// query sorts by them again, since SQL keeps no order through a subquery.
fn cast_to_text(
    rendered: &RenderedSql,
    columns: &[String],
    decoders: &[Option<Decoder>],
) -> String {
    let mut select: Vec<String> = columns
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
    select.extend(
        rendered
            .order_by
            .iter()
            .map(|c| format!("q.{}", quote_identifier(&c.alias))),
    );
    format!(
        "SELECT {}\nFROM (\n{}\n) q{}",
        select.join(", "),
        rendered.sql,
        rendered.order_by_clause(&PostgresDialect, "q")
    )
}

/// The solutions of one query, read one row at a time.
pub struct Solutions {
    /// `None` once exhausted, or when no row can match.
    rows: Option<Pin<Box<RowStream>>>,
    builder: SolutionBuilder,
    decoders: Vec<Decoder>,
    row_limit: u64,
    /// The row count's column, after the projected columns and the sort
    /// keys, which are never read; `None` when the statement has none
    /// because the row limit cannot cut the result ([`row_fetch`]).
    count_column: Option<usize>,
    returned: u64,
    truncated: bool,
    timeout: Duration,
}

impl SolutionStream for Solutions {
    type Error = Error;

    fn variables(&self) -> &[String] {
        self.builder.variables()
    }

    async fn try_next(&mut self) -> Result<Option<Solution>, Error> {
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
        if let Some(count_column) = self.count_column {
            if self.returned == 0 {
                let total: i64 = row
                    .try_get(count_column)
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

    fn is_truncated(&self) -> bool {
        self.truncated
    }
}

#[cfg(test)]
mod tests {
    use tokio_postgres::NoTls;
    use v12n_core::mapping::{Mapping, MappingIndex};
    use v12n_core::{sparql, unfold};

    use super::*;

    const SHOP: &str = include_str!("../../../examples/shop/mapping.ttl");

    /// The statement `execute` prepares for `query` over the shop under
    /// `row_limit`, with a column `decoders` cannot read when `cast`, and
    /// the plan's own SQL (cast when `cast`) that it may be.
    fn statements(query: &str, cast: bool, row_limit: u64) -> (String, String) {
        let mapping = Mapping::from_turtle(SHOP).unwrap();
        let query = sparql::parse(&format!("PREFIX ex: <http://example.org/>\n{query}")).unwrap();
        let plan = unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap();
        let rendered = plan.to_sql().unwrap();
        let decoders: Vec<Option<Decoder>> = plan
            .columns
            .iter()
            .enumerate()
            .map(|(i, _)| (i > 0).then_some(Decoder::Text))
            .collect();
        let decoders = cast.then_some(decoders.as_slice());
        let limited = row_fetch(row_limit, plan.limit).is_some();
        (
            statement_sql(&plan, &rendered, decoders, limited),
            statement_sql(&plan, &rendered, decoders, false),
        )
    }

    fn statement(query: &str, cast: bool, row_limit: u64) -> String {
        statements(query, cast, row_limit).0
    }

    #[test]
    fn row_fetch_reads_one_past_the_limit_only_when_it_can_cut() {
        for (row_limit, query_limit, fetch) in [
            (0, None, None),
            (0, Some(5), None),
            (2, None, Some(3)),
            (2, Some(0), None),
            (2, Some(1), None),
            (2, Some(2), None),
            (2, Some(3), Some(3)),
            (2, Some(usize::MAX), Some(3)),
            (u64::MAX, None, Some(i64::MAX)),
        ] {
            assert_eq!(
                row_fetch(row_limit, query_limit),
                fetch,
                "row limit {row_limit}, LIMIT {query_limit:?}"
            );
        }
    }

    /// SQL keeps no order through a subquery, so every query around one
    /// with `ORDER BY` sorts again by the selected keys, and the client
    /// reads the rows in the outermost query's order.
    #[test]
    fn every_wrapper_sorts_again_by_the_keys() {
        for (name, query, cast) in [
            (
                "ordered",
                "SELECT ?o ?a WHERE { ?o ex:amount ?a } ORDER BY DESC(?a)",
                false,
            ),
            (
                "ordered_and_sliced",
                "SELECT ?o ?a WHERE { ?o ex:amount ?a } ORDER BY DESC(?a) LIMIT 2 OFFSET 1",
                false,
            ),
            (
                "ordered_cast",
                "SELECT ?o ?a WHERE { ?o ex:amount ?a } ORDER BY DESC(?a) LIMIT 2",
                true,
            ),
            (
                "ordered_union",
                "SELECT ?x WHERE { { ?x ex:name ?v } UNION { ?x ex:amount ?v } } ORDER BY ?v",
                false,
            ),
            (
                "sliced",
                "SELECT ?o WHERE { ?o ex:amount ?a } LIMIT 2",
                false,
            ),
            ("plain", "SELECT ?o WHERE { ?o ex:amount ?a }", false),
        ] {
            // Every query `LIMIT` here is above the row limit.
            let sql = statement(query, cast, 1);
            insta::assert_snapshot!(name, sql, query);
        }
    }

    /// A query `LIMIT` at or below the row limit leaves no row to cut, so
    /// the statement is the plan's own SQL, without the row count; its
    /// outermost query still sorts. One more than the row limit keeps the
    /// row count.
    #[test]
    fn limit_within_the_row_limit_runs_the_plans_own_sql() {
        for (name, query, cast, row_limit) in [
            (
                "fits_sliced",
                "SELECT ?o WHERE { ?o ex:amount ?a } LIMIT 2",
                false,
                2,
            ),
            (
                "fits_ordered_and_sliced",
                "SELECT ?o ?a WHERE { ?o ex:amount ?a } ORDER BY DESC(?a) LIMIT 2 OFFSET 1",
                false,
                2,
            ),
            (
                "fits_ordered_cast",
                "SELECT ?o ?a WHERE { ?o ex:amount ?a } ORDER BY DESC(?a) LIMIT 2",
                true,
                10_000,
            ),
            (
                "fits_ordered_distinct",
                "SELECT DISTINCT ?a WHERE { ?o ex:amount ?a } ORDER BY ?a LIMIT 2",
                false,
                2,
            ),
            (
                "fits_ordered_union",
                "SELECT ?x WHERE { { ?x ex:name ?v } UNION { ?x ex:amount ?v } } ORDER BY ?v LIMIT 2",
                false,
                2,
            ),
        ] {
            let (sql, own) = statements(query, cast, row_limit);
            assert_eq!(sql, own, "{query}");
            assert!(!sql.contains("OVER ()"), "{sql}");
            if query.contains("ORDER BY") {
                let outermost = sql.rfind("\n)").map_or(sql.as_str(), |i| &sql[i..]);
                assert!(outermost.contains("\nORDER BY "), "{sql}");
            }
            insta::assert_snapshot!(name, sql, query);
        }
        for (name, query) in [
            (
                "over_ordered_distinct",
                "SELECT DISTINCT ?a WHERE { ?o ex:amount ?a } ORDER BY ?a LIMIT 3",
            ),
            (
                "over_ordered_union",
                "SELECT ?x WHERE { { ?x ex:name ?v } UNION { ?x ex:amount ?v } } ORDER BY ?v LIMIT 3",
            ),
        ] {
            let sql = statement(query, false, 2);
            assert!(sql.starts_with("SELECT q.*, count(*) OVER ()"), "{sql}");
            insta::assert_snapshot!(name, sql, query);
        }
    }

    /// The sort keys travel through the row count and are sorted by
    /// again, and the plan still reads the index on the sorted column and
    /// stops after the `LIMIT`. Runs against the `testenv` PostgreSQL when
    /// `V12N_TEST_DATABASE_URL` is set.
    #[tokio::test]
    async fn wrapped_statement_keeps_the_index_on_the_sorted_column() {
        let Ok(url) = std::env::var("V12N_TEST_DATABASE_URL") else {
            eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
            return;
        };
        let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
        tokio::spawn(connection);
        client
            .batch_execute(
                "DROP SCHEMA IF EXISTS v12n_wrap CASCADE;
                 CREATE SCHEMA v12n_wrap;
                 CREATE TABLE v12n_wrap.items (id int PRIMARY KEY, price int NOT NULL);
                 INSERT INTO v12n_wrap.items
                   SELECT n, (n * 7919) % 10007 FROM generate_series(1, 100000) n;
                 CREATE INDEX items_price ON v12n_wrap.items (price);
                 ANALYZE v12n_wrap.items;",
            )
            .await
            .unwrap();
        let mapping = Mapping::from_turtle(
            "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
             <#M> rr:logicalTable [ rr:tableName \"v12n_wrap.items\" ] ;\n\
               rr:subjectMap [ rr:template \"http://example.org/item/{id}\" ] ;\n\
               rr:predicateObjectMap [ rr:predicate <http://example.org/price> ; \
               rr:objectMap [ rr:column \"price\" ] ] .",
        )
        .unwrap();
        let query = sparql::parse(
            "SELECT ?i ?p WHERE { ?i <http://example.org/price> ?p } ORDER BY DESC(?p) LIMIT 3",
        )
        .unwrap();
        let plan = unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap();
        let database = PostgresBackend::connect(&url, Limits::default())
            .await
            .unwrap();
        let snapshot = database
            .load_catalog(&CatalogRequest::for_plan(&plan))
            .await
            .unwrap();
        let ready = CatalogPlan::new(&plan, std::sync::Arc::new(snapshot)).unwrap();
        let rendered = ready
            .plan()
            .to_sql_with(&PostgresDialect, &ready.render_options())
            .unwrap();
        let sql = statement_sql(ready.plan(), &rendered, None, true);
        let explain: Vec<String> = client
            .query(&format!("EXPLAIN ANALYZE {sql}"), &[&3i64, &10_001i64])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        client
            .batch_execute("DROP SCHEMA v12n_wrap CASCADE")
            .await
            .unwrap();
        let plan_text = explain.join("\n");
        assert!(
            plan_text.contains("Index Scan Backward using items_price"),
            "{sql}\n{plan_text}"
        );
        assert!(!plan_text.contains("Seq Scan"), "{sql}\n{plan_text}");
    }
}

// SPDX-License-Identifier: Apache-2.0
//! Runs SQL produced by `v12n-core` on PostgreSQL and streams rows back as
//! SPARQL solutions.

mod bind;
mod decode;
mod error;
mod tls;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures_util::TryStreamExt;
use futures_util::future::{join, join4};
use tls::Tls;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Config, RowStream, Statement};
use v12n_core::dialect::PostgresDialect;
use v12n_core::mapping::LogicalTable;
use v12n_core::optimize;
use v12n_core::solution::{Solution, SolutionBuilder, SqlValue};
use v12n_core::sql::{
    ColumnType, ColumnTypes, Comparison, RenderOptions, RenderedSql, SqlPlan, TableKeys, TermKey,
    TermNormal, column_name_probe, column_probe, column_type_probe, folded_identifier,
    quote_identifier, quote_table_name,
};

use bind::{Bound, bind, term_key, term_normal};
use decode::{Decoder, Raw, column_type};
pub use error::Error;

/// Default for [`Limits::statement_timeout`].
pub const DEFAULT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default for [`Limits::row_limit`].
pub const DEFAULT_ROW_LIMIT: u64 = 10_000;
/// Applied when the connection string sets no `connect_timeout`.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Most plans whose statements a connection remembers ([`Database::recent`]).
const RECENT_PLANS: usize = 64;

/// The unique keys of the tables named in `$1`, see [`Database::load_keys`].
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

/// Unique keys of a plan's tables, see [`Database::load_keys`].
struct Keys {
    /// Every key that always holds, for removing duplicates.
    unique: TableKeys,
    /// Those that identify a row by SQL `=`, for merging scans.
    mergeable: TableKeys,
}

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
    /// Whether the server encoding is UTF-8, where `COLLATE "C"` orders
    /// strings by code point.
    utf8: bool,
    /// [`KEYS_SQL`], prepared with the connection. Its text never changes,
    /// so only preparing it again would cost a round trip per query; it
    /// still runs on every query, so keys are always read fresh.
    keys_statement: Statement,
    /// The statements [`execute`](Self::execute) prepared for recent plans,
    /// by the plan's SQL rendered without database information. Only a
    /// guess of what to prepare: the next query of that plan prepares them
    /// in the same round trip that reads the catalog, and uses one only if
    /// it is exactly the statement the fresh catalog information gives. A
    /// stale guess costs a round trip, never a wrong result.
    recent: Mutex<HashMap<String, Vec<String>>>,
}

impl Database {
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
        let (row, keys_statement) = join(
            client.query_one(
                "SELECT set_config('statement_timeout', $1, false), \
                 set_config('default_transaction_read_only', 'on', false), \
                 current_setting('server_encoding')",
                &[&timeout_ms],
            ),
            client.prepare(KEYS_SQL),
        )
        .await;
        let row = row.map_err(|e| Error::from_postgres(e, timeout))?;
        let keys_statement = keys_statement.map_err(|e| Error::from_postgres(e, timeout))?;
        let encoding: String = row.get(2);
        Ok(Self {
            client,
            limits,
            utf8: encoding == "UTF8",
            keys_statement,
            recent: Mutex::new(HashMap::new()),
        })
    }

    /// Whether the connection is gone, e.g. after a server restart.
    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// Runs a plan. Rows are fetched as the returned [`Solutions`] are read.
    ///
    /// Everything that depends on the plan alone is sent in one pipelined
    /// round trip: the catalog query for keys, one statement that resolves
    /// column names and reports column types, and the statements prepared
    /// for the same plan last time. The statement is then prepared, unless
    /// one of those is it, and run.
    pub async fn execute(&self, plan: &SqlPlan) -> Result<Solutions, Error> {
        let timeout = self.limits.statement_timeout;
        let pg = |e| Error::from_postgres(e, timeout);
        // Rendering without database information fails where only the
        // column types tell whether a query is supported; such plans are
        // not remembered.
        let shape = plan
            .to_sql_with(&PostgresDialect, &RenderOptions::default())
            .ok()
            .map(|rendered| rendered.sql);
        let guesses = shape
            .as_ref()
            .and_then(|shape| {
                self.recent
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(shape)
                    .cloned()
            })
            .unwrap_or_default();
        let tables = keyed_tables(plan);
        let folded = plan.folded_columns();
        let mut probe_columns = folded.clone();
        let term_columns = plan.term_columns();
        // Whether the statement needs column types is known only after the
        // keys when they are read, so then they are probed in any case,
        // within the same round trip.
        if !tables.is_empty()
            || plan.distinct
            || plan.branches.len() > 1
            || plan.has_duplicates(None)
            || !term_columns.is_empty()
        {
            for column in term_columns.into_iter().chain(plan.selected_columns()) {
                if !probe_columns.contains(&column) {
                    probe_columns.push(column);
                }
            }
        }
        let probe_sql = (!probe_columns.is_empty()).then(|| column_probe(&probe_columns));
        let (keys, probe, first, second) = join4(
            self.load_keys_of(&tables),
            self.prepare_if(probe_sql.as_deref()),
            self.prepare_if(guesses.first().map(String::as_str)),
            self.prepare_if(guesses.get(1).map(String::as_str)),
        )
        .await;
        let mut prepared: Vec<(&str, Statement)> = guesses
            .iter()
            .zip([first, second])
            .filter_map(|(sql, statement)| Some((sql.as_str(), statement?.ok()?)))
            .collect();
        // A probe that fails is retried as the separate steps it combines,
        // so that an error is reported by the step it belongs to.
        let probe = probe.and_then(Result::ok);
        let plan = match &probe {
            _ if folded.is_empty() => Cow::Borrowed(plan),
            Some(probe) => {
                let resolved: HashMap<_, _> = folded
                    .iter()
                    .cloned()
                    .zip(probe.columns())
                    .map(|(column, c)| (column, c.name().to_owned()))
                    .collect();
                Cow::Owned(plan.with_resolved_columns(&resolved))
            }
            None => self.resolve_columns(plan).await.map_err(pg)?,
        };
        let probed_types: Option<ColumnTypes> = probe.map(|probe| {
            probe_columns
                .into_iter()
                .zip(probe.columns())
                .map(|((table, name), c)| {
                    let name = if folded_identifier(&name) {
                        quote_identifier(c.name())
                    } else {
                        name
                    };
                    ((table, name), column_type(c.type_()))
                })
                .collect()
        });
        let keys = keys.map_err(pg)?;
        let plan = match keys
            .as_ref()
            .and_then(|k| optimize::eliminate_self_joins(&plan, &k.mergeable))
        {
            Some(merged) => Cow::Owned(merged),
            None => plan,
        };
        let keys = keys.map(|k| k.unique);
        // `DISTINCT` and `UNION` compare values with `=`, so the types of
        // the columns they compare tell which need a key next to them.
        let removes_duplicates = plan.distinct || plan.has_duplicates(keys.as_ref());
        let typed = typed_columns(&plan, removes_duplicates);
        let column_types = if typed.is_empty() {
            None
        } else if let Some(types) = probed_types.as_ref().and_then(|probed| {
            typed
                .iter()
                .map(|column| Some((column.clone(), probed.get(column)?.clone())))
                .collect::<Option<ColumnTypes>>()
        }) {
            Some(types)
        } else {
            self.column_types(&plan, removes_duplicates)
                .await
                .map_err(pg)?
        };
        let plan = match &column_types {
            Some(types) if plan.branches.len() > 1 => Cow::Owned(plan.with_union_types(types)?),
            _ => plan,
        };
        let plan = plan.as_ref();
        let options = RenderOptions {
            text_params: &[],
            keyed_params: &[],
            normal_params: &[],
            column_types: column_types.as_ref(),
            keys: keys.as_ref(),
            non_utf8: !self.utf8,
        };
        let rendered = plan.to_sql_with(&PostgresDialect, &options)?;
        let fetch = row_fetch(self.limits.row_limit, plan.limit);
        let mut used = vec![statement_sql(plan, &rendered, None, fetch.is_some())];
        let mut statement = self
            .take_or_prepare(&mut prepared, &used[0])
            .await
            .map_err(pg)?;
        // The projected columns, then the sort keys, then the row count.
        // A query that selects neither selects a constant in their place.
        let data_columns = plan.columns.len();
        let placeholder = usize::from(data_columns == 0 && rendered.order_by.is_empty());
        let count_column = fetch.map(|_| data_columns + rendered.order_by.len() + placeholder);
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
        // `=` finds a zero constant equal to `-0` and a `bpchar` constant
        // equal to values with more padding, which are other terms.
        let keyed_params: Vec<(usize, TermKey)> = statement.params()[..rendered.params.len()]
            .iter()
            .zip(&rendered.params)
            .enumerate()
            .filter_map(|(i, (ty, value))| term_key(ty, value).map(|key| (i, key)))
            .collect();
        // `=` finds a midnight constant different from `24:00:00`, the same term.
        let normal_params: Vec<(usize, TermNormal)> = statement.params()[..rendered.params.len()]
            .iter()
            .zip(&rendered.params)
            .enumerate()
            .filter_map(|(i, (ty, value))| term_normal(ty, value).map(|normal| (i, normal)))
            .collect();
        let cast_columns = decoders.iter().any(Option::is_none);
        let rendered = if cast_columns
            || !text_params.is_empty()
            || !keyed_params.is_empty()
            || !normal_params.is_empty()
        {
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
            statement = self
                .take_or_prepare(&mut prepared, &sql)
                .await
                .map_err(pg)?;
            used.push(sql);
            rendered
        } else {
            rendered
        };
        if let Some(shape) = shape
            && guesses != used
        {
            let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
            if recent.len() >= RECENT_PLANS {
                recent.clear();
            }
            recent.insert(shape, used);
        }
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
    /// solutions (`SqlPlan::key_tables`) or that a branch scans more than
    /// once, read from the catalog on every query so that a dropped key is
    /// never trusted. `None` when keys cannot help.
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

    /// The keys of [`table_keys`](Self::table_keys) of the tables that a
    /// branch of the plan scans more than once (`self_join_tables`) and
    /// that identify a row by SQL `=`, so that
    /// `optimize::eliminate_self_joins` can merge such scans. `None` when
    /// no table is scanned twice.
    pub async fn merge_keys(&self, plan: &SqlPlan) -> Result<Option<TableKeys>, Error> {
        let timeout = self.limits.statement_timeout;
        let keys = self
            .load_keys(plan)
            .await
            .map_err(|e| Error::from_postgres(e, timeout))?;
        Ok(keys
            .filter(|_| !optimize::self_join_tables(plan).is_empty())
            .map(|k| k.mergeable))
    }

    async fn load_table_keys(
        &self,
        plan: &SqlPlan,
    ) -> Result<Option<TableKeys>, tokio_postgres::Error> {
        Ok(self.load_keys(plan).await?.map(|k| k.unique))
    }

    /// The keys of the tables of `key_tables` and `self_join_tables`, in
    /// one catalog query.
    ///
    /// A key merges two scans of its table only when equality on it is
    /// exactly equality of rows: every column is `NOT NULL` (a unique
    /// index allows several `NULL`s), the index compares with the column's
    /// own collation and the type's default operator class, as the SQL
    /// `=` the plan renders does, and the type compares by `=` without a
    /// cast or a term key (`ColumnType::comparison`, `ColumnType::term_key`).
    ///
    /// A key with a column whose distinct values can give the same term
    /// (`ColumnType::term_normal`) is left out entirely: two rows that
    /// differ only there give the same terms, so the key proves neither
    /// that the rows' solutions differ nor that a scan matches one row.
    async fn load_keys(&self, plan: &SqlPlan) -> Result<Option<Keys>, tokio_postgres::Error> {
        self.load_keys_of(&keyed_tables(plan)).await
    }

    async fn load_keys_of(
        &self,
        tables: &[LogicalTable],
    ) -> Result<Option<Keys>, tokio_postgres::Error> {
        if tables.is_empty() {
            return Ok(None);
        }
        let names: Vec<String> = tables
            .iter()
            .map(|t| match t {
                LogicalTable::TableName(name) => quote_table_name(name),
                LogicalTable::SqlQuery(_) => unreachable!("only tables have keys"),
            })
            .collect();
        let rows = self.client.query(&self.keys_statement, &[&names]).await?;
        let mut keys = Keys {
            unique: TableKeys::new(),
            mergeable: TableKeys::new(),
        };
        for row in rows {
            let position: i32 = row.get(0);
            let columns: Vec<String> = row.get(1);
            let exact_index: bool = row.get(2);
            let types: Vec<u32> = row.get(3);
            let table = &tables[usize::try_from(position - 1).expect("ordinality starts at 1")];
            let types: Vec<Option<ColumnType>> = types
                .into_iter()
                .map(|oid| Type::from_oid(oid).map(|ty| column_type(&ty)))
                .collect();
            if types.iter().flatten().any(|ty| ty.term_normal.is_some()) {
                continue;
            }
            let exact_types = types.iter().all(|ty| {
                ty.as_ref()
                    .is_some_and(|ty| ty.comparison == Comparison::Value && ty.term_key.is_none())
            });
            if exact_index && exact_types {
                keys.mergeable
                    .entry(table.clone())
                    .or_default()
                    .push(columns.clone());
            }
            keys.unique.entry(table.clone()).or_default().push(columns);
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
    /// with `UNION`, and of every selected column when the query removes
    /// duplicates, read by preparing a statement that selects them.
    /// `None` when there are none.
    async fn column_types(
        &self,
        plan: &SqlPlan,
        removes_duplicates: bool,
    ) -> Result<Option<ColumnTypes>, tokio_postgres::Error> {
        let columns = typed_columns(plan, removes_duplicates);
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

    async fn prepare_if(
        &self,
        sql: Option<&str>,
    ) -> Option<Result<Statement, tokio_postgres::Error>> {
        match sql {
            Some(sql) => Some(self.client.prepare(sql).await),
            None => None,
        }
    }

    /// The statement for `sql`: one of `prepared` if it has that text, which
    /// was prepared within the same query, or a new one.
    async fn take_or_prepare(
        &self,
        prepared: &mut Vec<(&str, Statement)>,
        sql: &str,
    ) -> Result<Statement, tokio_postgres::Error> {
        match prepared.iter().position(|(text, _)| *text == sql) {
            Some(i) => Ok(prepared.swap_remove(i).1),
            None => self.client.prepare(sql).await,
        }
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

/// The tables whose keys [`Database::load_keys`] reads for `plan`: those of
/// `key_tables` and `self_join_tables`, each once.
fn keyed_tables(plan: &SqlPlan) -> Vec<LogicalTable> {
    let mut tables = plan.key_tables();
    for table in optimize::self_join_tables(plan) {
        if !tables.contains(&table) {
            tables.push(table);
        }
    }
    tables
}

/// The columns whose types [`Database::column_types`] reads: those the
/// plan compares as RDF terms or combines with `UNION`, and every selected
/// column when the query removes duplicates.
fn typed_columns(plan: &SqlPlan, removes_duplicates: bool) -> Vec<(LogicalTable, String)> {
    let mut columns = plan.term_columns();
    let selected = if removes_duplicates {
        plan.selected_columns()
    } else {
        plan.union_columns()
    };
    for column in selected {
        if !columns.contains(&column) {
            columns.push(column);
        }
    }
    columns
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

    /// Whether the result has more rows than the row limit. Known once
    /// the first solution has been read.
    pub fn is_truncated(&self) -> bool {
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
        let database = Database::connect(&url, Limits::default()).await.unwrap();
        let types = database.column_types(&plan, false).await.unwrap();
        let keys = database.load_table_keys(&plan).await.unwrap();
        let rendered = plan
            .to_sql_with(
                &PostgresDialect,
                &RenderOptions {
                    column_types: types.as_ref(),
                    keys: keys.as_ref(),
                    ..RenderOptions::default()
                },
            )
            .unwrap();
        let sql = statement_sql(&plan, &rendered, None, true);
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

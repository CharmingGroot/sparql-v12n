// SPDX-License-Identifier: Apache-2.0
//! Reads a [`CatalogSnapshot`] and checks that the database still matches
//! one. The catalog queries and one probe statement per logical table go
//! out in one pipelined round trip; only a table whose probe names a
//! missing column takes a second one, a probe per column, to tell which
//! columns are missing.

use std::collections::{HashMap, HashSet};

use futures_util::future::{join, join_all, join4};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, Row, Statement};
use v12n_core::catalog::{
    CatalogPlan, CatalogRequest, CatalogSnapshot, Column, Fingerprint, ForeignKey, Relation,
    TableCatalog, TableRequest, TableStatus, UniqueKey,
};
use v12n_core::mapping::LogicalTable;
use v12n_core::sql::{quote_table_name, table_probe};

use super::KEYS_SQL;
use crate::decode::column_type;

/// Per table named in `$1`: whether it exists, and a hash of the catalog
/// rows that keys, column types, names and foreign keys are read from
/// (Discussion #187). Non-unique indexes are left out, since they do not
/// change results. A table that does not exist hashes one constant line,
/// so that creating it changes the fingerprint.
const FINGERPRINT_SQL: &str = "WITH t AS (
   SELECT n::int4, to_regclass(name) AS rel
   FROM unnest($1::text[]) WITH ORDINALITY AS t(name, n))
 SELECT t.n, t.rel IS NOT NULL,
   encode(sha256(convert_to(string_agg(l.line, E'\\n' ORDER BY l.line), 'UTF8')), 'hex')
 FROM t CROSS JOIN LATERAL (
   SELECT concat_ws(',', 'r', t.rel::oid, c.relkind,
     (SELECT count(*) FROM pg_inherits h WHERE h.inhparent = t.rel)) AS line
   FROM (SELECT) one LEFT JOIN pg_class c ON c.oid = t.rel
   UNION ALL
   SELECT concat_ws(',', 'a', a.attnum, a.attname, a.atttypid, a.atttypmod, a.attnotnull,
     a.attcollation)
   FROM pg_attribute a
   WHERE a.attrelid = t.rel AND a.attnum > 0 AND NOT a.attisdropped
   UNION ALL
   SELECT concat_ws(',', 'i', i.indexrelid, i.indimmediate, i.indisvalid, i.indpred IS NULL,
     i.indexprs IS NULL, i.indnkeyatts, i.indkey, i.indcollation, i.indclass)
   FROM pg_index i WHERE i.indrelid = t.rel AND i.indisunique
   UNION ALL
   SELECT concat_ws(',', 'f', co.oid, co.confrelid, co.conkey, co.confkey, co.convalidated,
     co.condeferrable)
   FROM pg_constraint co WHERE co.conrelid = t.rel AND co.contype = 'f') l
 GROUP BY t.n, t.rel";

/// The `NOT NULL` columns of the tables named in `$1`.
const NOT_NULL_SQL: &str = "SELECT t.n::int4, a.attname::text
 FROM unnest($1::text[]) WITH ORDINALITY AS t(name, n)
 JOIN pg_attribute a ON a.attrelid = to_regclass(t.name)
 WHERE a.attnum > 0 AND NOT a.attisdropped AND a.attnotnull";

/// The validated foreign keys of the tables named in `$1`. A key added
/// `NOT VALID` does not hold for the rows that were already there.
const FOREIGN_KEYS_SQL: &str = "SELECT t.n::int4,
   array(SELECT a.attname::text FROM unnest(co.conkey) WITH ORDINALITY AS k(attnum, ord)
     JOIN pg_attribute a ON a.attrelid = co.conrelid AND a.attnum = k.attnum ORDER BY k.ord),
   co.confrelid::regclass::text,
   array(SELECT a.attname::text FROM unnest(co.confkey) WITH ORDINALITY AS k(attnum, ord)
     JOIN pg_attribute a ON a.attrelid = co.confrelid AND a.attnum = k.attnum ORDER BY k.ord)
 FROM unnest($1::text[]) WITH ORDINALITY AS t(name, n)
 JOIN pg_constraint co ON co.conrelid = to_regclass(t.name)
 WHERE co.contype = 'f' AND co.convalidated
 ORDER BY t.n, co.conname";

/// The catalog queries, prepared with the connection, so that reading a
/// snapshot takes one round trip and the fingerprint check on every query
/// shares the round trip that prepares the statement.
pub(crate) struct CatalogStatements {
    fingerprints: Statement,
    not_null: Statement,
    keys: Statement,
    foreign_keys: Statement,
}

impl CatalogStatements {
    pub(crate) async fn prepare(client: &Client) -> Result<Self, tokio_postgres::Error> {
        let (fingerprints, not_null, keys, foreign_keys) = join4(
            client.prepare(FINGERPRINT_SQL),
            client.prepare(NOT_NULL_SQL),
            client.prepare(KEYS_SQL),
            client.prepare(FOREIGN_KEYS_SQL),
        )
        .await;
        Ok(Self {
            fingerprints: fingerprints?,
            not_null: not_null?,
            keys: keys?,
            foreign_keys: foreign_keys?,
        })
    }
}

pub(crate) async fn load(
    client: &Client,
    statements: &CatalogStatements,
    request: &CatalogRequest,
) -> Result<CatalogSnapshot, tokio_postgres::Error> {
    let named: Vec<&LogicalTable> = request
        .tables()
        .iter()
        .map(|t| &t.table)
        .filter(|t| matches!(t, LogicalTable::TableName(_)))
        .collect();
    let (facts, probes) = join(
        TableFacts::read(client, statements, &named),
        join_all(request.tables().iter().map(|t| probe(client, t))),
    )
    .await;
    let mut facts = facts?;
    let mut entries = Vec::with_capacity(probes.len());
    for (requested, probe) in request.tables().iter().zip(probes) {
        let facts = facts.remove(&requested.table);
        let entry = TableCatalog {
            fingerprint: facts.as_ref().and_then(|f| f.fingerprint.clone()),
            status: status(probe?, facts),
        };
        entries.push((requested.table.clone(), entry));
    }
    Ok(CatalogSnapshot::new(entries))
}

/// Whether the database still matches the catalog entries `plan` was
/// completed from: the fingerprint of each table, and the result columns
/// of each `rr:sqlQuery`, whose dependencies the catalog does not list.
pub(crate) async fn is_current(
    client: &Client,
    statements: &CatalogStatements,
    plan: &CatalogPlan,
) -> Result<bool, tokio_postgres::Error> {
    let mut names = Vec::new();
    let mut expected = Vec::new();
    let mut queries = Vec::new();
    for (table, entry) in plan.entries() {
        match (table, &entry.fingerprint, &entry.status) {
            (LogicalTable::TableName(name), Some(fingerprint), _) => {
                names.push(quote_table_name(name));
                expected.push(fingerprint);
            }
            (LogicalTable::SqlQuery(_), _, TableStatus::Readable(relation)) => {
                queries.push((table, relation));
            }
            // The engine runs no plan over a table the snapshot shows
            // missing or unreadable.
            _ => return Ok(false),
        }
    }
    let read_fingerprints = async {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        client.query(&statements.fingerprints, &[&names]).await
    };
    let (rows, queries) = join(
        read_fingerprints,
        join_all(
            queries
                .into_iter()
                .map(|(table, relation)| same_columns(client, table, relation)),
        ),
    )
    .await;
    let fingerprints_match = rows?.iter().all(|row| {
        let position: i32 = row.get(0);
        let found: String = row.get(2);
        usize::try_from(position - 1)
            .ok()
            .and_then(|i| expected.get(i))
            .is_some_and(|expected| expected.as_str() == found)
    });
    let mut queries_match = true;
    for same in queries {
        queries_match &= same?;
    }
    Ok(fingerprints_match && queries_match)
}

/// Whether the logical table still has the columns `relation` shows, with
/// the same names and types.
async fn same_columns(
    client: &Client,
    table: &LogicalTable,
    relation: &Relation,
) -> Result<bool, tokio_postgres::Error> {
    let (written, columns): (Vec<String>, Vec<&Column>) = relation
        .columns
        .iter()
        .map(|(written, column)| (written.clone(), column))
        .unzip();
    match client.prepare(&table_probe(table, &written)).await {
        Ok(statement) => Ok(statement
            .columns()
            .iter()
            .zip(columns)
            .all(|(c, column)| c.name() == column.name && column_type(c.type_()) == column.ty)),
        Err(error) if error.code().is_some() => Ok(false),
        Err(error) => Err(error),
    }
}

/// `facts` is `None` for an `rr:sqlQuery`, which the catalog does not list.
fn status(probe: Probe, facts: Option<TableFacts>) -> TableStatus {
    let facts = match facts {
        Some(facts) if !facts.exists => return TableStatus::Missing,
        facts => facts.unwrap_or_default(),
    };
    match probe {
        Probe::Refused(message) => TableStatus::Unreadable(message),
        Probe::Columns { found, missing } => TableStatus::Readable(Relation {
            columns: found
                .into_iter()
                .map(|(written, name, ty)| {
                    let column = Column {
                        not_null: facts.not_null.contains(&name),
                        ty: column_type(&ty),
                        name,
                    };
                    (written, column)
                })
                .collect(),
            missing_columns: missing,
            unique_keys: facts.unique_keys,
            foreign_keys: facts.foreign_keys,
        }),
    }
}

/// What the catalog queries report about one `rr:tableName`.
#[derive(Default)]
struct TableFacts {
    exists: bool,
    fingerprint: Option<Fingerprint>,
    not_null: HashSet<String>,
    unique_keys: Vec<UniqueKey>,
    foreign_keys: Vec<ForeignKey>,
}

impl TableFacts {
    async fn read(
        client: &Client,
        statements: &CatalogStatements,
        tables: &[&LogicalTable],
    ) -> Result<HashMap<LogicalTable, Self>, tokio_postgres::Error> {
        if tables.is_empty() {
            return Ok(HashMap::new());
        }
        let names: Vec<String> = tables
            .iter()
            .map(|t| match t {
                LogicalTable::TableName(name) => quote_table_name(name),
                LogicalTable::SqlQuery(_) => unreachable!("only tables are in the catalog"),
            })
            .collect();
        let (fingerprints, not_null, keys, foreign_keys) = join4(
            client.query(&statements.fingerprints, &[&names]),
            client.query(&statements.not_null, &[&names]),
            client.query(&statements.keys, &[&names]),
            client.query(&statements.foreign_keys, &[&names]),
        )
        .await;
        let mut facts: Vec<Self> = tables.iter().map(|_| Self::default()).collect();
        for row in fingerprints? {
            let table = Self::of(&mut facts, &row);
            table.exists = row.get(1);
            table.fingerprint = Some(Fingerprint::new(row.get(2)));
        }
        for row in not_null? {
            Self::of(&mut facts, &row).not_null.insert(row.get(1));
        }
        for row in keys? {
            let types: Vec<u32> = row.get(3);
            let key = UniqueKey {
                columns: row.get(1),
                exact_index: row.get(2),
                types: types
                    .into_iter()
                    .map(|oid| Type::from_oid(oid).map(|ty| column_type(&ty)))
                    .collect(),
            };
            Self::of(&mut facts, &row).unique_keys.push(key);
        }
        for row in foreign_keys? {
            let key = ForeignKey {
                columns: row.get(1),
                referenced_table: row.get(2),
                referenced_columns: row.get(3),
            };
            Self::of(&mut facts, &row).foreign_keys.push(key);
        }
        Ok(tables.iter().map(|&t| t.clone()).zip(facts).collect())
    }

    /// The facts of the table a row is about: its first column is the
    /// table's position in `$1`, counted from 1.
    fn of<'f>(facts: &'f mut [Self], row: &Row) -> &'f mut Self {
        let position: i32 = row.get(0);
        &mut facts[usize::try_from(position - 1).expect("ordinality starts at 1")]
    }
}

/// What preparing a probe of a logical table's columns showed.
enum Probe {
    /// Each found column as the mapping writes it, as the database stores
    /// it, and its type; then the columns that do not exist.
    Columns {
        found: Vec<(String, String, Type)>,
        missing: Vec<String>,
    },
    /// The database's message for why it cannot read the table.
    Refused(String),
}

async fn probe(client: &Client, request: &TableRequest) -> Result<Probe, tokio_postgres::Error> {
    let error = match client
        .prepare(&table_probe(&request.table, &request.columns))
        .await
    {
        Ok(statement) => {
            let found = request
                .columns
                .iter()
                .zip(statement.columns())
                .map(|(written, c)| (written.clone(), c.name().to_owned(), c.type_().clone()))
                .collect();
            return Ok(Probe::Columns {
                found,
                missing: Vec::new(),
            });
        }
        Err(error) => error,
    };
    match error.code() {
        Some(&SqlState::UNDEFINED_COLUMN) => probe_each_column(client, request).await,
        Some(_) => Ok(Probe::Refused(database_message(&error))),
        None => Err(error),
    }
}

async fn probe_each_column(
    client: &Client,
    request: &TableRequest,
) -> Result<Probe, tokio_postgres::Error> {
    let statements = join_all(request.columns.iter().map(|column| {
        let sql = table_probe(&request.table, std::slice::from_ref(column));
        async move { client.prepare(&sql).await }
    }))
    .await;
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for (written, statement) in request.columns.iter().zip(statements) {
        match statement {
            Ok(statement) => {
                let c = &statement.columns()[0];
                found.push((written.clone(), c.name().to_owned(), c.type_().clone()));
            }
            Err(error) if error.code() == Some(&SqlState::UNDEFINED_COLUMN) => {
                missing.push(written.clone());
            }
            Err(error) if error.code().is_some() => {
                return Ok(Probe::Refused(database_message(&error)));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Probe::Columns { found, missing })
}

fn database_message(error: &tokio_postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |db| db.message().to_owned())
}

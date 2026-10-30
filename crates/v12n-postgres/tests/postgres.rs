// SPDX-License-Identifier: Apache-2.0
//! Runs against the `testenv` PostgreSQL. Skipped unless
//! `V12N_TEST_DATABASE_URL` is set, e.g.
//! `postgresql://v12n:v12n-dev@127.0.0.1:55432/shop`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio_postgres::NoTls;
use tokio_postgres::config::Host;
use tokio_postgres::types::Type;
use v12n_core::Limits;
use v12n_core::backend::{Backend, SolutionStream};
use v12n_core::catalog::{CatalogPlan, CatalogRequest};
use v12n_core::dialect::PostgresDialect;
use v12n_core::mapping::{BaseIri, LogicalTable, Mapping, MappingIndex};
use v12n_core::solution::canonical_decimal;
use v12n_core::sql::{
    ColumnType, ColumnTypes, Comparison, Lexical, RenderOptions, SqlPlan, TableKeys, TermKey,
    TermNormal,
};
use v12n_core::{Engine, Planner, sparql, unfold};
use v12n_postgres::{Error, PostgresBackend, Solutions};

const SHOP: &str = include_str!("../../../examples/shop/mapping.ttl");
const VIP_ORDERS: &str = include_str!("../../../examples/shop/queries/vip-orders.rq");

fn database_url() -> Option<String> {
    let url = std::env::var("V12N_TEST_DATABASE_URL").ok();
    if url.is_none() {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
    }
    url
}

/// Runs `plan` on the catalog entries of its tables, read just before.
async fn execute(database: &PostgresBackend, plan: &SqlPlan) -> Result<Solutions, Error> {
    let snapshot = database
        .load_catalog(&CatalogRequest::for_plan(plan))
        .await?;
    database
        .execute(&CatalogPlan::new(plan, Arc::new(snapshot))?)
        .await
}

fn plan(mapping: &str, query: &str) -> SqlPlan {
    let mapping = Mapping::from_turtle(mapping).unwrap();
    let query = sparql::parse(query).unwrap();
    unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap()
}

/// Each solution as `?var=term` pairs in N-Triples syntax, sorted, since
/// SPARQL results have no order without `ORDER BY`.
async fn run(url: &str, limits: Limits, plan: &SqlPlan) -> Result<(Vec<String>, bool), Error> {
    let (mut rows, truncated) = run_in_order(url, limits, plan).await?;
    rows.sort();
    Ok((rows, truncated))
}

/// Like [`run`], in the order the solutions arrive.
async fn run_in_order(
    url: &str,
    limits: Limits,
    plan: &SqlPlan,
) -> Result<(Vec<String>, bool), Error> {
    let database = PostgresBackend::connect(url, limits).await?;
    let mut solutions = execute(&database, plan).await?;
    let mut rows = Vec::new();
    while let Some(solution) = solutions.try_next().await? {
        let row: Vec<String> = solutions
            .variables()
            .iter()
            .zip(solution.values())
            .filter_map(|(v, t)| t.as_ref().map(|t| format!("?{v}={t}")))
            .collect();
        rows.push(row.join(" "));
    }
    Ok((rows, solutions.is_truncated()))
}

#[tokio::test]
async fn shop_vip_orders_returns_three_orders() {
    let Some(url) = database_url() else { return };
    let (rows, truncated) = run(&url, Limits::default(), &plan(SHOP, VIP_ORDERS))
        .await
        .unwrap();
    let int = "<http://www.w3.org/2001/XMLSchema#integer>";
    assert_eq!(
        rows,
        [
            format!("?order=<http://example.org/order/1001> ?amount=\"120000\"^^{int}"),
            format!("?order=<http://example.org/order/1003> ?amount=\"87000\"^^{int}"),
            format!("?order=<http://example.org/order/1004> ?amount=\"15000\"^^{int}"),
        ]
    );
    assert!(!truncated);
}

#[tokio::test]
async fn constant_subject_binds_an_integer_parameter_that_uses_the_index() {
    let Some(url) = database_url() else { return };
    let query = "SELECT ?n WHERE { <http://example.org/customer/7> <http://example.org/name> ?n }";
    let plan = plan(SHOP, query);
    let (rows, _) = run(&url, Limits::default(), &plan).await.unwrap();
    assert_eq!(rows, ["?n=\"Kim\""]);

    // The prepared statement takes the parameter type from `id`, so the
    // comparison stays on the integer column and can use its primary key,
    // which also shows that no duplicates need removing.
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    let database = PostgresBackend::connect(&url, Limits::default())
        .await
        .unwrap();
    let keys = database.table_keys(&plan).await.unwrap();
    let sql = plan
        .to_sql_with(
            &PostgresDialect,
            &RenderOptions {
                keys: keys.as_ref(),
                ..RenderOptions::default()
            },
        )
        .unwrap()
        .sql;
    let statement = client.prepare(&sql).await.unwrap();
    assert_eq!(statement.params(), [Type::INT4]);
    client
        .batch_execute("SET enable_seqscan = off")
        .await
        .unwrap();
    let explain: Vec<String> = client
        .query(&format!("EXPLAIN {sql}"), &[&7i32])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert!(
        explain[0].starts_with("Index Scan using customers_pkey"),
        "{explain:?}"
    );
}

#[tokio::test]
async fn constant_without_natural_lexical_form_matches_nothing() {
    let Some(url) = database_url() else { return };
    let query =
        "SELECT ?n WHERE { <http://example.org/customer/007> <http://example.org/name> ?n }";
    let (rows, _) = run(&url, Limits::default(), &plan(SHOP, query))
        .await
        .unwrap();
    assert!(rows.is_empty(), "{rows:?}");
}

#[tokio::test]
async fn row_limit_cuts_off_and_reports_truncation() {
    let Some(url) = database_url() else { return };
    let query = "SELECT ?c WHERE { ?c a <http://example.org/Customer> }";
    let plan = plan(SHOP, query);
    let limits = |row_limit| Limits {
        row_limit,
        ..Limits::default()
    };
    let (rows, truncated) = run(&url, limits(2), &plan).await.unwrap();
    assert_eq!((rows.len(), truncated), (2, true));
    let (rows, truncated) = run(&url, limits(3), &plan).await.unwrap();
    assert_eq!((rows.len(), truncated), (3, false));
    let (rows, truncated) = run(&url, limits(0), &plan).await.unwrap();
    assert_eq!((rows.len(), truncated), (3, false));
}

#[tokio::test]
async fn truncation_is_known_after_the_first_solution() {
    let Some(url) = database_url() else { return };
    let plan = plan(
        SHOP,
        "SELECT ?c WHERE { ?c a <http://example.org/Customer> }",
    );
    for (row_limit, truncated) in [(1, true), (2, true), (3, false), (0, false)] {
        let limits = Limits {
            row_limit,
            ..Limits::default()
        };
        let database = PostgresBackend::connect(&url, limits).await.unwrap();
        let mut solutions = execute(&database, &plan).await.unwrap();
        assert!(solutions.try_next().await.unwrap().is_some());
        assert_eq!(solutions.is_truncated(), truncated, "row limit {row_limit}");
    }
}

/// Discussion #22 at the edges of the row limit (3 here): exactly 3
/// solutions are complete, a fourth one is truncation, and a query `LIMIT`
/// at or below the row limit, which runs without the row count, is never
/// truncated, with or without `ORDER BY`, `OFFSET`, `DISTINCT` or `UNION`.
#[tokio::test]
async fn truncation_at_the_edges_of_the_row_limit_and_a_query_limit() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        "SELECT n AS id, n % 2 AS k FROM generate_series(1, 5) n",
        &["k"],
    );
    let limits = Limits {
        row_limit: 3,
        ..Limits::default()
    };
    let ordered = "SELECT ?row WHERE { ?row ex:k ?k } ORDER BY DESC(?row)";
    let distinct = "SELECT DISTINCT ?row ?k WHERE { ?row ex:k ?k } ORDER BY DESC(?row)";
    let union = "SELECT ?row WHERE { { ?row ex:k 0 } UNION { ?row ex:k 1 } } ORDER BY DESC(?row)";
    for (body, modifiers, expected, truncated) in [
        (ordered, "", &[5, 4, 3][..], true),
        (ordered, "LIMIT 0", &[], false),
        (ordered, "LIMIT 2", &[5, 4], false),
        (ordered, "LIMIT 3", &[5, 4, 3], false),
        (ordered, "LIMIT 4", &[5, 4, 3], true),
        (ordered, "OFFSET 2", &[3, 2, 1], false),
        (ordered, "LIMIT 3 OFFSET 1", &[4, 3, 2], false),
        (ordered, "LIMIT 4 OFFSET 1", &[4, 3, 2], true),
        (ordered, "LIMIT 4 OFFSET 2", &[3, 2, 1], false),
        (ordered, "LIMIT 3 OFFSET 4", &[1], false),
        (distinct, "LIMIT 3", &[5, 4, 3], false),
        (distinct, "LIMIT 4", &[5, 4, 3], true),
        (union, "LIMIT 3", &[5, 4, 3], false),
        (union, "LIMIT 4", &[5, 4, 3], true),
    ] {
        let query = format!("{body} {modifiers}");
        let (rows, cut) = ordered_rows(&url, &mapping, &query, limits).await;
        assert_eq!((rows.as_slice(), cut), (expected, truncated), "{query}");
    }
    for (modifiers, rows, truncated) in [("LIMIT 3", 3, false), ("LIMIT 4", 3, true)] {
        let query = format!(
            "PREFIX ex: <http://example.org/>\nSELECT ?row WHERE {{ ?row ex:k ?k }} {modifiers}"
        );
        let (solutions, cut) = run(&url, limits, &plan(&mapping, &query)).await.unwrap();
        assert_eq!((solutions.len(), cut), (rows, truncated), "{modifiers}");
    }
}

fn sql_query_mapping(sql: &str, columns: &[&str]) -> String {
    let objects: String = columns
        .iter()
        .map(|c| format!("  rr:predicateObjectMap [ rr:predicate ex:{c} ; rr:objectMap [ rr:column \"{c}\" ] ] ;\n"))
        .collect();
    format!(
        "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
         @prefix ex: <http://example.org/> .\n\
         <#Values> rr:logicalTable [ rr:sqlQuery \"\"\"{sql}\"\"\" ] ;\n\
         {objects}  rr:subjectMap [ rr:template \"http://example.org/row/{{id}}\" ] .\n"
    )
}

/// Arithmetic in FILTER (#99): computed in `numeric`, so `int8` cannot
/// overflow; a division by zero and a `numeric` NaN are errors, which drop
/// the row under `!` too.
#[tokio::test]
async fn filter_arithmetic_follows_xpath_numbers() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        "SELECT * FROM (VALUES \
           (1, 7::int4, 2::int8, 1.5::numeric), \
           (2, -3::int4, 0::int8, 'NaN'::numeric), \
           (3, 4::int4, 9223372036854775807::int8, 2.25::numeric)) v(id, a, b, d)",
        &["a", "b", "d"],
    );
    let rows = |filter: &'static str| {
        let (url, mapping) = (url.clone(), mapping.clone());
        async move { filtered_rows(&url, &mapping, "?row ex:a ?a ; ex:b ?b ; ex:d ?d", filter).await }
    };
    // 7 / 2 is the decimal 3.5, not the integer 3.
    assert_eq!(rows("?a / ?b = 3.5").await, ["1"]);
    // Row 2 divides by zero: an error, so neither the filter nor its
    // negation keeps it.
    assert_eq!(rows("?a / ?b > 0").await, ["1", "3"]);
    assert_eq!(rows("!(?a / ?b > 0)").await, Vec::<String>::new());
    // int8's largest value plus one does not overflow.
    assert_eq!(rows("?b + 1 > 9223372036854775807").await, ["3"]);
    assert_eq!(rows("?b * ?b > 0").await, ["1", "3"]);
    // A numeric NaN is not an xsd:decimal: an error.
    assert_eq!(rows("?d * 2 > 0").await, ["1", "3"]);
    assert_eq!(rows("!(?d * 2 > 0)").await, Vec::<String>::new());
    assert_eq!(rows("?d - ?a = -5.5").await, ["1"]);
    // Effective boolean value: true unless zero.
    assert_eq!(rows("?a + ?b - 9").await, ["2", "3"]);
}

/// `BIND` and `SELECT (expr AS ?v)` (#100): numbers in canonical form, of
/// the class XPath gives them; an error, such as a division by zero or a
/// `numeric` NaN, leaves the variable unbound and keeps the solution.
#[tokio::test]
async fn bind_and_select_expressions_follow_xpath_numbers() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        "SELECT * FROM (VALUES \
           (1, 7::int4, 2::int8, 1.50::numeric), \
           (2, -3::int4, 0::int8, 'NaN'::numeric), \
           (3, 4::int4, 9223372036854775807::int8, 2.25::numeric)) v(id, a, b, d)",
        &["a", "b", "d"],
    );
    let query = "PREFIX ex: <http://example.org/>
        SELECT ?row (?a / ?b AS ?q) (?b + 1 AS ?next) (?d * 2 AS ?dd) ?copy ?k
        WHERE { ?row ex:a ?a ; ex:b ?b ; ex:d ?d BIND(?a AS ?copy) BIND(\"k\" AS ?k) }";
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap();
    let xsd = |lexical: &str, datatype: &str| {
        format!("\"{lexical}\"^^<http://www.w3.org/2001/XMLSchema#{datatype}>")
    };
    let row = |id: u8, terms: &[(&str, String)]| {
        let mut out = format!("?row=<http://example.org/row/{id}>");
        for (variable, term) in terms {
            out.push_str(&format!(" ?{variable}={term}"));
        }
        out
    };
    assert_eq!(
        rows,
        [
            row(
                1,
                &[
                    ("q", xsd("3.5", "decimal")),
                    ("next", xsd("3", "integer")),
                    ("dd", xsd("3", "decimal")),
                    ("copy", xsd("7", "integer")),
                    ("k", "\"k\"".to_owned()),
                ]
            ),
            row(
                2,
                &[
                    ("next", xsd("1", "integer")),
                    ("copy", xsd("-3", "integer")),
                    ("k", "\"k\"".to_owned()),
                ]
            ),
            row(
                3,
                &[
                    // The precision of a decimal division is up to the
                    // implementation (XPath op:numeric-divide); PostgreSQL
                    // picks the scale of `numeric` division.
                    (
                        "q",
                        xsd("0.000000000000000000433680868994201774", "decimal")
                    ),
                    ("next", xsd("9223372036854775808", "integer")),
                    ("dd", xsd("4.5", "decimal")),
                    ("copy", xsd("4", "integer")),
                    ("k", "\"k\"".to_owned()),
                ]
            ),
        ]
    );
}

#[tokio::test]
async fn natural_mapping_covers_postgres_types() {
    let Some(url) = database_url() else { return };
    let columns = [
        "d", "di", "f", "f30", "r", "b", "dt", "tm", "ts", "tz", "bin", "s", "u", "iv",
    ];
    let mapping = sql_query_mapping(
        r"SELECT 1 AS id, 12.50::numeric AS d, 3.00::numeric AS di, 1.5::float8 AS f,
          30::float8 AS f30, 0.1::real AS r,
          true AS b, DATE '2024-02-29' AS dt, TIME '10:00:00.25' AS tm,
          TIMESTAMP '2024-01-02 03:04:05.5' AS ts,
          TIMESTAMPTZ '2024-01-02 03:04:05+09' AS tz, '\\x0a0b'::bytea AS bin,
          'a b'::varchar AS s, '00000000-0000-0000-0000-00000000000a'::uuid AS u,
          INTERVAL '1 day' AS iv",
        &columns,
    );
    let patterns: String = columns.iter().map(|c| format!("ex:{c} ?{c} ; ")).collect();
    let query = format!(
        "PREFIX ex: <http://example.org/> SELECT {} WHERE {{ ?row {patterns} }}",
        columns.map(|c| format!("?{c}")).join(" ")
    );
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, &query))
        .await
        .unwrap();
    let xsd = |lexical: &str, datatype: &str| {
        format!("\"{lexical}\"^^<http://www.w3.org/2001/XMLSchema#{datatype}>")
    };
    let expected = [
        ("d", xsd("12.5", "decimal")),
        // XML Schema 1.1 decimalCanonicalMap: an integer has no point.
        ("di", xsd("3", "decimal")),
        ("f", xsd("1.5E0", "double")),
        ("f30", xsd("3.0E1", "double")),
        ("r", xsd("1.0E-1", "double")),
        ("b", xsd("true", "boolean")),
        ("dt", xsd("2024-02-29", "date")),
        ("tm", xsd("10:00:00.25", "time")),
        ("ts", xsd("2024-01-02T03:04:05.5", "dateTime")),
        ("tz", xsd("2024-01-01T18:04:05Z", "dateTime")),
        ("bin", xsd("0A0B", "hexBinary")),
        ("s", "\"a b\"".to_owned()),
        ("u", "\"00000000-0000-0000-0000-00000000000a\"".to_owned()),
        // No natural datatype: the value cast to a string (R2RML §10.2).
        ("iv", "\"1 day\"".to_owned()),
    ]
    .map(|(v, t)| format!("?{v}={t}"))
    .join(" ");
    assert_eq!(rows, [expected]);
}

#[tokio::test]
async fn statement_timeout_cancels_the_query() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping("SELECT 1 AS id, 2 AS v FROM pg_sleep(2)", &["v"]);
    let query = "PREFIX ex: <http://example.org/> SELECT ?v WHERE { ?s ex:v ?v }";
    let limits = Limits {
        statement_timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let error = run(&url, limits, &plan(&mapping, query)).await.unwrap_err();
    assert!(matches!(error, Error::Timeout(_)), "{error}");
}

#[tokio::test]
async fn session_is_read_only() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping("SELECT id, id AS v FROM customers FOR UPDATE", &["v"]);
    let query = "PREFIX ex: <http://example.org/> SELECT ?v WHERE { ?s ex:v ?v }";
    let error = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Database(m) if m.contains("read-only transaction")),
        "{error}"
    );
}

/// Subjects matched by `?row <predicate> <object>`, sorted.
async fn subjects(url: &str, mapping: &str, predicate: &str, object: &str) -> Vec<String> {
    let query = format!("SELECT ?row WHERE {{ ?row <http://example.org/{predicate}> {object} }}");
    match run(url, Limits::default(), &plan(mapping, &query)).await {
        Ok((rows, _)) => rows,
        Err(e) => panic!("{query}: {e}"),
    }
}

#[tokio::test]
async fn out_of_range_and_non_canonical_constants_match_nothing_without_an_error() {
    let Some(url) = database_url() else { return };
    let xsd = "http://www.w3.org/2001/XMLSchema#";
    // `amount` is an integer column holding 120000 for order 1001.
    assert_eq!(
        subjects(&url, SHOP, "amount", "120000").await,
        ["?row=<http://example.org/order/1001>"]
    );
    for object in [
        "99999999999".to_owned(),
        "120000.0".to_owned(),
        "\"120000\"".to_owned(),
        format!("\"120000\"^^<{xsd}decimal>"),
        format!("\"0120000\"^^<{xsd}integer>"),
        format!("\"+120000\"^^<{xsd}integer>"),
    ] {
        assert!(
            subjects(&url, SHOP, "amount", &object).await.is_empty(),
            "{object}"
        );
    }
    // Template values: `id` is an integer column with customer 7.
    for id in ["99999999999", "7.0", "007", "+7", "seven"] {
        let query = format!(
            "SELECT ?n WHERE {{ <http://example.org/customer/{id}> <http://example.org/name> ?n }}"
        );
        let (rows, _) = run(&url, Limits::default(), &plan(SHOP, &query))
            .await
            .unwrap_or_else(|e| panic!("{id}: {e}"));
        assert!(rows.is_empty(), "{id}: {rows:?}");
    }
    // A plain literal is not the integer a natural integer column yields,
    // and a text column yields no integers.
    assert!(subjects(&url, SHOP, "name", "7").await.is_empty());
}

#[tokio::test]
async fn unrepresentable_constant_drops_only_its_branch() {
    let Some(url) = database_url() else { return };
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#Text> rr:logicalTable [ rr:sqlQuery "SELECT 1 AS id, 'abc'::text AS v" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:v ; rr:objectMap [ rr:column "v" ] ] .
        <#Int> rr:logicalTable [ rr:sqlQuery "SELECT 2 AS id, 5::int4 AS v" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:v ; rr:objectMap [ rr:column "v" ] ] .
    "#;
    assert_eq!(
        subjects(&url, mapping, "v", "\"abc\"").await,
        ["?row=<http://example.org/row/1>"]
    );
    assert_eq!(
        subjects(&url, mapping, "v", "5").await,
        ["?row=<http://example.org/row/2>"]
    );
    assert!(subjects(&url, mapping, "v", "\"5\"").await.is_empty());
    assert!(subjects(&url, mapping, "v", "99999999999").await.is_empty());
}

#[tokio::test]
async fn constants_round_trip_through_the_result_formatting() {
    let Some(url) = database_url() else { return };
    let columns = [
        "d", "di", "f", "b", "dt", "ts", "tz", "bin", "u", "iv", "i2", "i8",
    ];
    let mapping = sql_query_mapping(
        r"SELECT 1 AS id, 12.50::numeric AS d, 3.00::numeric AS di, 1.5::float8 AS f, true AS b,
          DATE '2024-02-29' AS dt, TIMESTAMP '2024-01-02 03:04:05.5' AS ts,
          TIMESTAMPTZ '2024-01-02 03:04:05+09' AS tz, '\\x0a0b'::bytea AS bin,
          '00000000-0000-0000-0000-00000000000a'::uuid AS u,
          INTERVAL '1 day' AS iv, 5::int2 AS i2, 99999999999::int8 AS i8",
        &columns,
    );
    let xsd = |lexical: &str, datatype: &str| {
        format!("\"{lexical}\"^^<http://www.w3.org/2001/XMLSchema#{datatype}>")
    };
    let uuid = "00000000-0000-0000-0000-00000000000a";
    let cases = [
        ("d", "12.5".to_owned(), true),
        ("d", "12.50".to_owned(), false),
        ("d", "\"12.5\"".to_owned(), false),
        ("di", xsd("3", "decimal"), true),
        ("di", "3.0".to_owned(), false),
        ("di", "3".to_owned(), false),
        ("f", "1.5E0".to_owned(), true),
        ("f", xsd("1.5", "double"), false),
        ("b", "true".to_owned(), true),
        ("b", xsd("1", "boolean"), false),
        ("dt", xsd("2024-02-29", "date"), true),
        ("dt", xsd("2024-2-29", "date"), false),
        // PostgreSQL rejects these as input instead of matching nothing.
        ("dt", xsd("2024-02-30", "date"), false),
        ("dt", xsd("soon", "date"), false),
        ("ts", xsd("2024-01-02T03:04:05.5", "dateTime"), true),
        ("ts", xsd("2024-01-02 03:04:05.5", "dateTime"), false),
        ("tz", xsd("2024-01-01T18:04:05Z", "dateTime"), true),
        ("tz", xsd("2024-01-02T03:04:05+09:00", "dateTime"), false),
        ("bin", xsd("0A0B", "hexBinary"), true),
        ("bin", xsd("0a0b", "hexBinary"), false),
        ("u", format!("\"{uuid}\""), true),
        ("u", format!("\"{}\"", uuid.to_uppercase()), false),
        ("u", "\"abc\"".to_owned(), false),
        // No natural datatype: compared with the column's text form.
        ("iv", "\"1 day\"".to_owned(), true),
        ("iv", "\"24 hours\"".to_owned(), false),
        ("iv", "\"abc\"".to_owned(), false),
        ("i2", "5".to_owned(), true),
        ("i2", "40000".to_owned(), false),
        ("i8", "99999999999".to_owned(), true),
    ];
    for (column, object, expected) in cases {
        let rows = subjects(&url, &mapping, column, &object).await;
        assert_eq!(!rows.is_empty(), expected, "{column} {object}: {rows:?}");
    }
}

#[tokio::test]
async fn numeric_template_matches_the_canonical_form_of_any_scale() {
    let Some(url) = database_url() else { return };
    // PostgreSQL prints 12.50 with its scale; the IRI has the canonical 12.5.
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#Price> rr:logicalTable [ rr:sqlQuery "SELECT 12.50::numeric AS p, 'x' AS v" ] ;
          rr:subjectMap [ rr:template "http://example.org/price/{p}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:v ; rr:objectMap [ rr:column "v" ] ] .
    "#;
    let (rows, _) = run(
        &url,
        Limits::default(),
        &plan(mapping, "SELECT ?p WHERE { ?p <http://example.org/v> ?v }"),
    )
    .await
    .unwrap();
    assert_eq!(rows, ["?p=<http://example.org/price/12.5>"]);
    for (iri, expected) in [("12.5", true), ("12.50", false), ("12.500", false)] {
        let query = format!(
            "SELECT ?v WHERE {{ <http://example.org/price/{iri}> <http://example.org/v> ?v }}"
        );
        let (rows, _) = run(&url, Limits::default(), &plan(mapping, &query))
            .await
            .unwrap();
        assert_eq!(!rows.is_empty(), expected, "{iri}");
    }
}

/// One triples map per `(name, sql, predicate-object maps)`, all with the
/// subject `http://example.org/row/{id}`.
fn views(rows: &[(&str, &str, &str)]) -> String {
    let maps: String = rows
        .iter()
        .map(|(name, sql, poms)| {
            format!(
                "<#{name}> rr:logicalTable [ rr:sqlQuery \"\"\"{sql}\"\"\" ] ;\n\
                 rr:subjectMap [ rr:template \"http://example.org/row/{{id}}\" ] ;\n{poms} .\n"
            )
        })
        .collect();
    format!(
        "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
         @prefix ex: <http://example.org/> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n{maps}"
    )
}

#[tokio::test]
async fn constant_literal_matches_only_the_natural_datatype_of_the_column() {
    let Some(url) = database_url() else { return };
    let v = "rr:predicateObjectMap [ rr:predicate ex:v ; rr:objectMap [ rr:column \"v\" ] ]";
    let mapping = views(&[
        ("int", "SELECT 1 AS id, 7::int4 AS v", v),
        ("text", "SELECT 2 AS id, '7'::text AS v", v),
    ]);
    let xsd = "http://www.w3.org/2001/XMLSchema#";
    assert_eq!(
        subjects(&url, &mapping, "v", "7").await,
        ["?row=<http://example.org/row/1>"]
    );
    assert_eq!(
        subjects(&url, &mapping, "v", "\"7\"").await,
        ["?row=<http://example.org/row/2>"]
    );
    for object in [format!("\"7\"^^<{xsd}decimal>"), "7.0".to_owned()] {
        assert!(
            subjects(&url, &mapping, "v", &object).await.is_empty(),
            "{object}"
        );
    }
}

#[tokio::test]
async fn join_across_column_types_compares_terms_not_values() {
    let Some(url) = database_url() else { return };
    let v = |p: &str, extra: &str| {
        format!(
            "rr:predicateObjectMap [ rr:predicate ex:{p} ; rr:objectMap [ rr:column \"v\" {extra}] ]"
        )
    };
    let mapping = views(&[
        ("int", "SELECT 1 AS id, 7::int4 AS v", &v("left", "")),
        ("bigint", "SELECT 2 AS id, 7::int8 AS v", &v("right", "")),
        // "7" is not "7"^^xsd:integer, and 7.0 is "7"^^xsd:decimal.
        ("text", "SELECT 3 AS id, '7'::text AS v", &v("right", "")),
        (
            "numeric",
            "SELECT 4 AS id, 7.0::numeric AS v",
            &v("right", ""),
        ),
        // rr:datatype makes the text "7" the term "7"^^xsd:integer.
        (
            "typed",
            "SELECT 5 AS id, '7'::text AS v",
            &v("right", "; rr:datatype xsd:integer "),
        ),
    ]);
    let query = "PREFIX ex: <http://example.org/>
        SELECT ?a ?b WHERE { ?a ex:left ?x . ?b ex:right ?x }";
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            "?a=<http://example.org/row/1> ?b=<http://example.org/row/2>",
            "?a=<http://example.org/row/1> ?b=<http://example.org/row/5>",
        ]
    );
}

#[tokio::test]
async fn string_comparison_applies_iri_safe_encoding() {
    let Some(url) = database_url() else { return };
    let mapping = views(&[
        (
            "link",
            "SELECT 1 AS id, 'http://example.org/doc/a%20b%2Fc' AS target \
             UNION ALL SELECT 2, 'http://example.org/flag/true'",
            "rr:predicateObjectMap [ rr:predicate ex:links ;
               rr:objectMap [ rr:column \"target\" ; rr:termType rr:IRI ] ]",
        ),
        (
            "doc",
            "SELECT 3 AS id, 'a b/c'::text AS name",
            "rr:predicateObjectMap [ rr:predicate ex:about ;
               rr:objectMap [ rr:template \"http://example.org/doc/{name}\" ] ]",
        ),
        (
            "flag",
            "SELECT 4 AS id, true AS b",
            "rr:predicateObjectMap [ rr:predicate ex:about ;
               rr:objectMap [ rr:template \"http://example.org/flag/{b}\" ] ]",
        ),
        (
            "pair",
            "SELECT 5 AS id, 'x y'::text AS a, 'z'::text AS b",
            "rr:predicateObjectMap [ rr:predicate ex:about ;
               rr:objectMap [ rr:template \"http://example.org/pair/{a}-{b}\" ] ]",
        ),
    ]);
    // A column holding IRIs against a template.
    let query = "PREFIX ex: <http://example.org/>
        SELECT ?l ?s WHERE { ?l ex:links ?t . ?s ex:about ?t }";
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            "?l=<http://example.org/row/1> ?s=<http://example.org/row/3>",
            "?l=<http://example.org/row/2> ?s=<http://example.org/row/4>",
        ]
    );
    // A constant against a template whose values cannot be read off it.
    assert_eq!(
        subjects(&url, &mapping, "about", "<http://example.org/pair/x%20y-z>").await,
        ["?row=<http://example.org/row/5>"]
    );
    assert!(
        subjects(
            &url,
            &mapping,
            "about",
            "<http://example.org/pair/x%2520y-z>"
        )
        .await
        .is_empty()
    );
}

#[tokio::test]
async fn base_iri_resolves_relative_iris_in_results_constants_and_joins() {
    let Some(url) = database_url() else { return };
    let mapping = views(&[
        (
            "page",
            "SELECT 1 AS id, 'a.html'::text AS iri \
             UNION ALL SELECT 2, 'http://example.com/base/a.html' \
             UNION ALL SELECT 3, 'http://other.example/b' \
             UNION ALL SELECT 4, 'urn:x'",
            "rr:predicateObjectMap [ rr:predicate ex:links ;
               rr:objectMap [ rr:column \"iri\" ; rr:termType rr:IRI ] ]",
        ),
        (
            "number",
            "SELECT 6 AS id, 7::int4 AS n",
            "rr:predicateObjectMap [ rr:predicate ex:links ;
               rr:objectMap [ rr:column \"n\" ; rr:termType rr:IRI ] ]",
        ),
        (
            "person",
            "SELECT 5 AS id, 'a.html'::text AS name",
            "rr:predicateObjectMap [ rr:predicate ex:about ;
               rr:objectMap [ rr:template \"{name}\" ; rr:termType rr:IRI ] ]",
        ),
    ]);
    let base = BaseIri::new("http://example.com/base/").unwrap();
    let plan_with_base = |query: &str| {
        let mapping = Mapping::from_turtle(&mapping)
            .unwrap()
            .with_base_iri(base.clone());
        let query = sparql::parse(&format!("PREFIX ex: <http://example.org/>\n{query}")).unwrap();
        unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap()
    };
    let rows = |query: &'static str| {
        let plan = plan_with_base(query);
        let url = url.clone();
        async move { run(&url, Limits::default(), &plan).await.unwrap().0 }
    };
    let row = |n: u32, iri: &str| format!("?r=<http://example.org/row/{n}> ?t=<{iri}>");
    assert_eq!(
        rows("SELECT ?r ?t WHERE { ?r ex:links ?t }").await,
        [
            row(1, "http://example.com/base/a.html"),
            row(2, "http://example.com/base/a.html"),
            row(3, "http://other.example/b"),
            row(4, "urn:x"),
            row(6, "http://example.com/base/7"),
        ]
    );
    // The relative and the absolute value both produce the constant. The
    // integer column can hold neither form, so its branch is empty.
    assert_eq!(
        rows("SELECT ?r WHERE { ?r ex:links <http://example.com/base/a.html> }").await,
        [
            "?r=<http://example.org/row/1>",
            "?r=<http://example.org/row/2>"
        ]
    );
    // Only the relative form fits the integer column.
    assert_eq!(
        rows("SELECT ?r WHERE { ?r ex:links <http://example.com/base/7> }").await,
        ["?r=<http://example.org/row/6>"]
    );
    assert!(
        rows("SELECT ?r WHERE { ?r ex:links <http://example.com/base/urn:x> }")
            .await
            .is_empty()
    );
    // A relative template value joins both forms of the column.
    assert_eq!(
        rows("SELECT ?r ?s WHERE { ?r ex:links ?t . ?s ex:about ?t }").await,
        [
            "?r=<http://example.org/row/1> ?s=<http://example.org/row/5>",
            "?r=<http://example.org/row/2> ?s=<http://example.org/row/5>",
        ]
    );

    // Without a base IRI the relative value is a data error naming the setting.
    let query = "PREFIX ex: <http://example.org/>
        SELECT ?t WHERE { ?r ex:links ?t }";
    let error = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("V12N_BASE_IRI"), "{error}");
}

#[tokio::test]
async fn numeric_compares_with_other_types_by_its_canonical_form() {
    let Some(url) = database_url() else { return };
    let mapping = views(&[
        (
            "price",
            "SELECT 1 AS id, 7.00::numeric AS p UNION ALL SELECT 2, 7.50::numeric",
            "rr:predicateObjectMap [ rr:predicate ex:item ;
               rr:objectMap [ rr:template \"http://example.org/item/{p}\" ] ]",
        ),
        (
            "item",
            "SELECT 3 AS id, 'http://example.org/item/7' AS iri, 7 AS n \
             UNION ALL SELECT 4, 'http://example.org/item/7.5', 8 \
             UNION ALL SELECT 5, 'http://example.org/item/7.00', 9",
            "rr:predicateObjectMap [ rr:predicate ex:same ;
               rr:objectMap [ rr:column \"iri\" ; rr:termType rr:IRI ] ] ;
             rr:predicateObjectMap [ rr:predicate ex:item ;
               rr:objectMap [ rr:template \"http://example.org/item/{n}\" ] ]",
        ),
    ]);
    // A numeric template against a column holding IRIs; row 3's integer
    // template matches its own IRI too.
    let query = "SELECT ?p ?q WHERE {
        ?p <http://example.org/item> ?i . ?q <http://example.org/same> ?i }";
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            "?p=<http://example.org/row/1> ?q=<http://example.org/row/3>",
            "?p=<http://example.org/row/2> ?q=<http://example.org/row/4>",
            "?p=<http://example.org/row/3> ?q=<http://example.org/row/3>",
        ]
    );
    // Same template shape, numeric against integer: 7.00 is 7.
    let query = "SELECT ?p ?q WHERE {
        ?p <http://example.org/item> ?i . ?q <http://example.org/item> ?i }";
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap();
    let pair =
        |p: u8, q: u8| format!("?p=<http://example.org/row/{p}> ?q=<http://example.org/row/{q}>");
    assert_eq!(
        rows,
        [
            pair(1, 1),
            pair(1, 3),
            pair(2, 2),
            pair(3, 1),
            pair(3, 3),
            pair(4, 4),
            pair(5, 5)
        ]
    );
}

/// A variable that reads columns of different SQL types in different
/// UNION branches gets each branch's natural datatype (#24).
#[tokio::test]
async fn union_keeps_the_natural_datatype_of_each_branch() {
    let Some(url) = database_url() else { return };
    let v = "rr:predicateObjectMap [ rr:predicate ex:v ; rr:objectMap [ rr:column \"v\" ] ]";
    let mapping = views(&[
        ("int", "SELECT 1 AS id, 7::int4 AS v", v),
        ("bigint", "SELECT 2 AS id, 7::int8 AS v", v),
        ("text", "SELECT 3 AS id, '7'::text AS v", v),
        ("numeric", "SELECT 4 AS id, 7.50::numeric AS v", v),
        ("real", "SELECT 5 AS id, 0.1::real AS v", v),
        ("date", "SELECT 6 AS id, DATE '2024-02-29' AS v", v),
        (
            "uuid",
            "SELECT 7 AS id, '00000000-0000-0000-0000-00000000000a'::uuid AS v",
            v,
        ),
        ("interval", "SELECT 8 AS id, INTERVAL '1 day' AS v", v),
        ("varchar", "SELECT 9 AS id, '7'::varchar AS v", v),
    ]);
    let xsd = |lexical: &str, datatype: &str| {
        format!("\"{lexical}\"^^<http://www.w3.org/2001/XMLSchema#{datatype}>")
    };
    let query = "SELECT ?row ?v WHERE { ?row <http://example.org/v> ?v }";
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap();
    let mut expected = [
        (1, xsd("7", "integer")),
        (2, xsd("7", "integer")),
        (3, "\"7\"".to_owned()),
        (4, xsd("7.5", "decimal")),
        (5, xsd("1.0E-1", "double")),
        (6, xsd("2024-02-29", "date")),
        (7, "\"00000000-0000-0000-0000-00000000000a\"".to_owned()),
        (8, "\"1 day\"".to_owned()),
        (9, "\"7\"".to_owned()),
    ]
    .map(|(id, v)| format!("?row=<http://example.org/row/{id}> ?v={v}"));
    expected.sort();
    assert_eq!(rows, expected);

    // Equal terms from columns of different types are one solution.
    let query = "SELECT DISTINCT ?v WHERE { ?row <http://example.org/v> ?v }";
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap();
    let mut expected = [
        xsd("7", "integer"),
        "\"7\"".to_owned(),
        xsd("7.5", "decimal"),
        xsd("1.0E-1", "double"),
        xsd("2024-02-29", "date"),
        "\"00000000-0000-0000-0000-00000000000a\"".to_owned(),
        "\"1 day\"".to_owned(),
    ]
    .map(|v| format!("?v={v}"));
    expected.sort();
    assert_eq!(rows, expected);
}

/// Template values from columns of different types give the same IRI
/// when their natural lexical forms are equal, so the same triple from
/// two triples maps is still one solution.
#[tokio::test]
async fn union_compares_template_values_of_different_types_as_terms() {
    let Some(url) = database_url() else { return };
    let v = "rr:predicateObjectMap [ rr:predicate ex:v ; rr:objectMap [ rr:column \"v\" ] ]";
    let mapping = views(&[
        ("int", "SELECT 1 AS id, 'x'::text AS v", v),
        ("text", "SELECT '1'::text AS id, 'x'::text AS v", v),
        ("numeric", "SELECT 1.00::numeric AS id, 'x'::text AS v", v),
        ("space", "SELECT 'a b'::text AS id, 'x'::text AS v", v),
        ("half", "SELECT 1.50::numeric AS id, 'x'::text AS v", v),
    ]);
    for query in [
        "SELECT ?row ?v WHERE { ?row <http://example.org/v> ?v }",
        "SELECT ?row WHERE { ?row <http://example.org/v> ?v }",
    ] {
        let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
            .await
            .unwrap();
        let v = if query.contains("?row ?v") {
            " ?v=\"x\""
        } else {
            ""
        };
        let mut expected =
            ["1", "1.5", "a%20b"].map(|id| format!("?row=<http://example.org/row/{id}>{v}"));
        expected.sort();
        assert_eq!(rows, expected, "{query}");
    }

    // The same for a variable that is not selected.
    let tag = "rr:predicateObjectMap [ rr:predicate ex:tag ;
        rr:objectMap [ rr:template \"http://example.org/tag/{t}\" ] ]";
    let mapping = views(&[
        ("int", "SELECT 1 AS id, 7 AS t", tag),
        ("text", "SELECT 1 AS id, '7'::text AS t", tag),
    ]);
    let query = "SELECT ?row WHERE { ?row <http://example.org/tag> ?t }";
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap();
    assert_eq!(rows, ["?row=<http://example.org/row/1>"]);

    // No SQL computes the canonical form of a double (Discussion #35).
    let mapping = views(&[
        ("int", "SELECT 1 AS id, 'x'::text AS v", v),
        ("double", "SELECT 1.5::float8 AS id, 'x'::text AS v", v),
    ]);
    let query = "SELECT ?row WHERE { ?row <http://example.org/v> ?v }";
    let error = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Core(v12n_core::Error::UnsupportedFeature(m)) if m.contains("float8")
        ),
        "{error}"
    );
}

#[tokio::test]
async fn comparison_sql_cannot_compute_is_rejected() {
    let Some(url) = database_url() else { return };
    let mapping = views(&[
        (
            "reading",
            "SELECT 1 AS id, 30::float8 AS f, TIME '10:00:00' AS tm",
            "rr:predicateObjectMap [ rr:predicate ex:item ;
               rr:objectMap [ rr:template \"http://example.org/item/{f}\" ] ] ;
             rr:predicateObjectMap [ rr:predicate ex:at ;
               rr:objectMap [ rr:template \"http://example.org/at/{tm}\" ] ]",
        ),
        (
            "item",
            "SELECT 2 AS id, 'http://example.org/item/3.0E1' AS iri, 7 AS n, \
             'http://example.org/at/10:00:00' AS at",
            "rr:predicateObjectMap [ rr:predicate ex:same ;
               rr:objectMap [ rr:column \"iri\" ; rr:termType rr:IRI ] ] ;
             rr:predicateObjectMap [ rr:predicate ex:same ;
               rr:objectMap [ rr:column \"at\" ; rr:termType rr:IRI ] ] ;
             rr:predicateObjectMap [ rr:predicate ex:item ;
               rr:objectMap [ rr:template \"http://example.org/item/{n}\" ] ]",
        ),
    ]);
    // Floating point and date/time have no SQL expression for their
    // canonical forms (Discussion #35).
    for (query, column) in [
        (
            "SELECT ?p WHERE { ?p <http://example.org/item> ?i . ?q <http://example.org/same> ?i }",
            "float8",
        ),
        (
            "SELECT ?p WHERE { ?p <http://example.org/item> ?i . ?q <http://example.org/item> ?i }",
            "float8",
        ),
        (
            "SELECT ?p WHERE { ?p <http://example.org/at> ?i . ?q <http://example.org/same> ?i }",
            "time",
        ),
    ] {
        let error = run(&url, Limits::default(), &plan(&mapping, query))
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                Error::Core(v12n_core::Error::UnsupportedFeature(m)) if m.contains(column)
            ),
            "{query}: {error}"
        );
    }
}

/// `trim_scale(x)::text`, which SQL uses for the natural lexical form of
/// a `numeric` (`Lexical::Decimal`), must be exactly the form the engine
/// writes for the value it decodes, and `canonical_decimal` of the value's
/// text output, for every value.
#[tokio::test]
async fn trim_scale_matches_the_canonical_decimal_formatter() {
    let Some(url) = database_url() else { return };
    let edge_cases = [
        "0",
        "0.000",
        "-0",
        "-0.00",
        "1",
        "-1",
        "3.00",
        "12.50",
        "-12.50",
        "100",
        "100.00",
        "0.5",
        "-0.5",
        ".5",
        "-.5",
        "0.0001000",
        "00012.3400",
        "10",
        "1000000",
        "1e20",
        "1e-30",
        "-1.5e-7",
        "120000.50",
        "-0.0012",
        "9999.9999",
        "10000.0000",
        "0.00010000",
        "123456789012345678901234567890.123456789000000",
        "-99999999999999999999999999999999999999.99999999999999999999",
        "NaN",
        "Infinity",
        "-Infinity",
    ];
    let sql = format!(
        "SELECT id, x AS d, x::text AS s, trim_scale(x)::text AS t
         FROM (
           SELECT i AS id, x FROM unnest('{{{}}}'::numeric[]) WITH ORDINALITY AS u(x, i)
           UNION ALL
           SELECT 100000 + i,
                  round((i * 7919 % 2000003 - 1000001)::numeric / 10::numeric ^ (i % 23), i % 31)
           FROM generate_series(1, 4000) AS i
           UNION ALL
           SELECT 200000 + i, round((i * 104729 % 1000003)::numeric * 10::numeric ^ (i % 40), -(i % 5))
           FROM generate_series(1, 1000) AS i
           UNION ALL
           SELECT 300000 + i, i::numeric / 7 FROM generate_series(-500, 500) AS i
           UNION ALL
           SELECT 400000 + i, (i::numeric / 4)::numeric(30, 10) FROM generate_series(-200, 200) AS i
           UNION ALL
           SELECT 500000 + i, round(1::numeric / 3, 1000) * i FROM generate_series(-3, 3) AS i
           UNION ALL
           SELECT 600000 + i, 10::numeric ^ i FROM generate_series(-60, 60) AS i
         ) AS v",
        edge_cases.join(",")
    );
    let mapping = sql_query_mapping(&sql, &["d", "s", "t"]);
    let query = "PREFIX ex: <http://example.org/>
        SELECT ?d ?s ?t WHERE { ?row ex:d ?d ; ex:s ?s ; ex:t ?t }";
    let limits = Limits {
        row_limit: 0,
        ..Limits::default()
    };
    let database = PostgresBackend::connect(&url, limits).await.unwrap();
    let mut solutions = execute(&database, &plan(&mapping, query)).await.unwrap();
    let mut checked = 0;
    while let Some(solution) = solutions.try_next().await.unwrap() {
        let [Some(d), Some(s), Some(t)] = solution.values() else {
            panic!("{solution:?}")
        };
        let (s, t) = (s.to_string(), t.to_string());
        let (s, t) = (s.trim_matches('"'), t.trim_matches('"'));
        assert_eq!(
            d.to_string(),
            format!("\"{t}\"^^<http://www.w3.org/2001/XMLSchema#decimal>"),
            "{s}"
        );
        // NaN and the infinities have no xsd:decimal form and stay as they are.
        assert_eq!(canonical_decimal(s).as_deref().unwrap_or(s), t, "{s}");
        checked += 1;
    }
    assert_eq!(
        checked,
        edge_cases.len() + 4000 + 1000 + 1001 + 401 + 7 + 121
    );
}

/// R2RML §5 with PostgreSQL's case folding: a regular identifier in the
/// mapping names the column PostgreSQL would find for it unquoted, a
/// delimited one the column with exactly that name.
#[tokio::test]
async fn unquoted_column_names_resolve_as_postgres_folds_them() {
    let Some(url) = database_url() else { return };
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#A> rr:logicalTable [ rr:sqlQuery """SELECT 1 AS Id, 'x' AS "Name", 'y' AS Label""" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{ID}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "\"Name\"" ] ] ;
          rr:predicateObjectMap [ rr:predicate ex:label ; rr:objectMap [ rr:column "LABEL" ] ] .
        <#B> rr:logicalTable [ rr:sqlQuery """SELECT '1' AS iD, 'z' AS "V" """ ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{Id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:v ; rr:objectMap [ rr:column "\"V\"" ] ] .
    "#;
    let rows = |variables: &str, pattern: &str| {
        let plan = plan(
            mapping,
            &format!("PREFIX ex: <http://example.org/> SELECT {variables} WHERE {{ {pattern} }}"),
        );
        let url = url.clone();
        async move { run(&url, Limits::default(), &plan).await.unwrap().0 }
    };
    assert_eq!(
        rows("?s ?n ?l", "?s ex:name ?n ; ex:label ?l").await,
        ["?s=<http://example.org/row/1> ?n=\"x\" ?l=\"y\""]
    );
    // A constant compared with a folded column is bound with its type.
    assert_eq!(
        rows("?l", "<http://example.org/row/1> ex:label ?l").await,
        ["?l=\"y\""]
    );
    // The join compares an integer and a text column, which needs their
    // types under the resolved names.
    assert_eq!(
        rows("?s ?l ?v", "?s ex:label ?l ; ex:v ?v").await,
        ["?s=<http://example.org/row/1> ?l=\"y\" ?v=\"z\""]
    );
}

/// R2RML §5: a delimited identifier that is not all lower case is not
/// equivalent to any regular identifier in PostgreSQL (W3C R2RMLTC0002f).
#[tokio::test]
async fn unquoted_name_does_not_match_a_mixed_case_column() {
    let Some(url) = database_url() else { return };
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#A> rr:logicalTable [ rr:sqlQuery """SELECT 1 AS id, 'x' AS "Name" """ ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "Name" ] ] .
    "#;
    let plan = plan(
        mapping,
        "SELECT ?n WHERE { ?s <http://example.org/name> ?n }",
    );
    let error = run(&url, Limits::default(), &plan).await.unwrap_err();
    assert!(
        matches!(&error, Error::Core(v12n_core::Error::Catalog(m)) if m.contains("has no column Name")),
        "{error}"
    );
}

/// An RDF graph is a set: a triple from duplicate rows, or from two
/// triples maps, is matched once (SPARQL 1.1 §18.5), while projecting
/// variables away keeps one solution per match.
#[tokio::test]
async fn duplicate_triples_count_once_and_projection_keeps_the_bag() {
    let Some(url) = database_url() else { return };
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#A> rr:logicalTable [ rr:sqlQuery """SELECT * FROM (VALUES (1, 'a'), (1, 'a'), (1, 'b'), (2, 'a')) v(id, name)""" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "name" ] ] .
        <#B> rr:logicalTable [ rr:sqlQuery "SELECT 2 AS id, 'a' AS name" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "name" ] ] .
    "#;
    let rows = |query: &str| {
        let plan = plan(
            mapping,
            &format!("PREFIX ex: <http://example.org/> {query}"),
        );
        let url = url.clone();
        async move { run(&url, Limits::default(), &plan).await.unwrap().0 }
    };
    let row = |id: u8| format!("?s=<http://example.org/row/{id}>");
    assert_eq!(
        rows("SELECT ?s ?n WHERE { ?s ex:name ?n }").await,
        [
            format!("{} ?n=\"a\"", row(1)),
            format!("{} ?n=\"b\"", row(1)),
            format!("{} ?n=\"a\"", row(2)),
        ]
    );
    // Two triples about row 1 give it twice; row 2 has one triple, from
    // both triples maps.
    assert_eq!(
        rows("SELECT ?s WHERE { ?s ex:name ?n }").await,
        [row(1), row(1), row(2)]
    );
    assert_eq!(
        rows("SELECT ?s WHERE { ?s ex:name \"a\" }").await,
        [row(1), row(2)]
    );
    assert_eq!(
        rows("SELECT ?n WHERE { ?s ex:name ?n . ?t ex:name ?n }")
            .await
            .len(),
        // ?n = "a": ?s and ?t each row 1 or 2; ?n = "b": row 1 only.
        5
    );
    assert_eq!(
        rows("SELECT DISTINCT ?s WHERE { ?s ex:name ?n }").await,
        [row(1), row(2)]
    );
}

/// Unique keys are read from the catalog only when they count: unique,
/// valid, immediate, not partial, on plain columns, only the key columns
/// of an index with `INCLUDE`, and not on a table with inheritance
/// children (partitions are fine).
#[tokio::test]
async fn table_keys_come_from_unique_indexes_that_always_hold() {
    let Some(url) = database_url() else { return };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            r#"DROP SCHEMA IF EXISTS v12n_keys CASCADE;
            CREATE SCHEMA v12n_keys;
            CREATE TABLE v12n_keys.pk (id int PRIMARY KEY, v text);
            CREATE TABLE v12n_keys.composite (a int, b int, v text, PRIMARY KEY (a, b), UNIQUE (v));
            CREATE TABLE v12n_keys.partial (id int, v text);
            CREATE UNIQUE INDEX ON v12n_keys.partial (id) WHERE id > 0;
            CREATE TABLE v12n_keys.expression (id int, v text);
            CREATE UNIQUE INDEX ON v12n_keys.expression (lower(v));
            CREATE TABLE v12n_keys.deferrable (id int UNIQUE DEFERRABLE, v text);
            CREATE TABLE v12n_keys.include (id int, v text);
            CREATE UNIQUE INDEX ON v12n_keys.include (id) INCLUDE (v);
            CREATE TABLE v12n_keys."Mixed" ("Id" int PRIMARY KEY, v text);
            CREATE TABLE v12n_keys.parent (id int PRIMARY KEY, v text);
            CREATE TABLE v12n_keys.child () INHERITS (v12n_keys.parent);
            CREATE TABLE v12n_keys.partitioned (id int PRIMARY KEY, v text) PARTITION BY RANGE (id);
            CREATE TABLE v12n_keys.partition PARTITION OF v12n_keys.partitioned FOR VALUES FROM (0) TO (10);
            CREATE TABLE v12n_keys.none (id int, v text);
            CREATE VIEW v12n_keys.view AS SELECT * FROM v12n_keys.pk;"#,
        )
        .await
        .unwrap();
    let database = PostgresBackend::connect(&url, Limits::default())
        .await
        .unwrap();
    let mut keys = Vec::new();
    for table in [
        "v12n_keys.pk",
        "v12n_keys.composite",
        "v12n_keys.partial",
        "v12n_keys.expression",
        "v12n_keys.deferrable",
        "v12n_keys.include",
        r#"v12n_keys.\"Mixed\""#,
        "v12n_keys.parent",
        "v12n_keys.partitioned",
        "v12n_keys.none",
        "v12n_keys.view",
        "v12n_keys.missing",
    ] {
        let mapping = format!(
            "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
             <#M> rr:logicalTable [ rr:tableName \"{table}\" ] ;\n\
               rr:subjectMap [ rr:template \"http://example.org/{{v}}\" ] ;\n\
               rr:predicateObjectMap [ rr:predicate <http://example.org/v> ; rr:objectMap [ rr:column \"v\" ] ] ."
        );
        let plan = plan(&mapping, "SELECT ?s WHERE { ?s <http://example.org/v> ?v }");
        let mut found: Vec<Vec<String>> = database
            .table_keys(&plan)
            .await
            .unwrap()
            .expect("one table scan can use keys")
            .into_values()
            .flatten()
            .collect();
        found.sort();
        keys.push(found);
    }
    client
        .batch_execute("DROP SCHEMA v12n_keys CASCADE")
        .await
        .unwrap();
    let k = |columns: &[&str]| columns.iter().map(|c| c.to_string()).collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            vec![k(&["id"])],
            vec![k(&["a", "b"]), k(&["v"])],
            vec![],
            vec![],
            vec![],
            vec![k(&["id"])],
            vec![k(&["Id"])],
            vec![],
            vec![k(&["id"])],
            vec![],
            vec![],
            vec![],
        ]
    );
}

/// OPTIONAL keeps every customer; a constant the column type cannot hold
/// makes only the optional part fail, not the whole query.
#[tokio::test]
async fn optional_keeps_the_rows_its_part_does_not_match() {
    let Some(url) = database_url() else { return };
    let query = |amount: &str| {
        format!(
            "PREFIX ex: <http://example.org/>
             SELECT ?c ?o WHERE {{ ?c a ex:Customer OPTIONAL {{ ?o ex:placedBy ?c ; ex:amount {amount} }} }}"
        )
    };
    let customer = |id: u32| format!("?c=<http://example.org/customer/{id}>");
    let (rows, _) = run(&url, Limits::default(), &plan(SHOP, &query("30000")))
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            customer(7),
            format!("{} ?o=<http://example.org/order/1002>", customer(8)),
            customer(9),
        ]
    );
    for unmatchable in ["99999999999", "7.5", "\"30000\""] {
        let (rows, _) = run(&url, Limits::default(), &plan(SHOP, &query(unmatchable)))
            .await
            .unwrap();
        assert_eq!(
            rows,
            [customer(7), customer(8), customer(9)],
            "{unmatchable}"
        );
    }
}

/// The link between the two sides of OPTIONAL compares RDF terms across
/// column types, like a join, and a UNION branch where the optional part
/// cannot match selects NULLs of the other branches' types.
#[tokio::test]
async fn optional_compares_terms_across_column_types_and_union_branches() {
    let Some(url) = database_url() else { return };
    let v = "rr:predicateObjectMap [ rr:predicate ex:v ; rr:objectMap [ rr:column \"v\" ] ]";
    let w = "rr:predicateObjectMap [ rr:predicate ex:w ; rr:objectMap [ rr:column \"w\" ] ]";
    let mapping = views(&[
        ("int", "SELECT 1 AS id, 'x'::text AS v", v),
        ("text", "SELECT '2'::text AS id, 'y'::text AS v", v),
        ("w", "SELECT 2 AS id, 7::int4 AS w", w),
    ]);
    let query = "PREFIX ex: <http://example.org/>
        SELECT ?row ?v ?w WHERE { ?row ex:v ?v OPTIONAL { ?row ex:w ?w } }";
    let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            "?row=<http://example.org/row/1> ?v=\"x\"".to_owned(),
            "?row=<http://example.org/row/2> ?v=\"y\" \
             ?w=\"7\"^^<http://www.w3.org/2001/XMLSchema#integer>"
                .to_owned(),
        ]
    );

    let class = |name: &str, sql: &str, extra: &str| {
        format!(
            "<#{name}> rr:logicalTable [ rr:sqlQuery \"\"\"{sql}\"\"\" ] ;
               rr:subjectMap [ rr:template \"http://example.org/{name}/{{id}}\" ; rr:class ex:C ] {extra} .\n"
        )
    };
    let mapping = format!(
        "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n@prefix ex: <http://example.org/> .\n{}{}{}",
        class("a", "SELECT 1 AS id", ""),
        class("b", "SELECT 1 AS id", ""),
        class("c", "SELECT 1 AS id, 7::int4 AS w", &format!("; {w}")),
    );
    let query = "PREFIX ex: <http://example.org/>
        SELECT DISTINCT ?w WHERE { ?row a ex:C OPTIONAL { ?row ex:w ?w } }";
    let plan = plan(&mapping, query);
    assert_eq!(plan.branches.len(), 3);
    let (rows, _) = run(&url, Limits::default(), &plan).await.unwrap();
    assert_eq!(
        rows,
        [
            String::new(),
            "?w=\"7\"^^<http://www.w3.org/2001/XMLSchema#integer>".to_owned()
        ]
    );
}

/// The `?row` of every solution, by its id.
async fn filtered_rows(url: &str, mapping: &str, patterns: &str, filter: &str) -> Vec<String> {
    let query = format!(
        "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT ?row WHERE {{ {patterns} FILTER({filter}) }}"
    );
    let (rows, _) = run(url, Limits::default(), &plan(mapping, &query))
        .await
        .unwrap_or_else(|e| panic!("{filter}: {e}"));
    rows.iter()
        .map(|r| {
            r.trim_start_matches("?row=<http://example.org/row/")
                .trim_end_matches('>')
                .to_owned()
        })
        .collect()
}

/// Values the operator mapping treats specially: NaN is never equal to or
/// ordered with anything, -0 equals 0, `numeric` NaN and infinities and
/// infinite timestamps are not values of their XSD datatypes, so comparing
/// them is an error, which stays an error under negation.
#[tokio::test]
async fn filter_comparisons_follow_xpath_on_special_values() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT * FROM (VALUES
            (1, 1.5::numeric, 1.5::float8, TIMESTAMP '2024-01-01 00:00:00'),
            (2, 'NaN', 'NaN', 'infinity'),
            (3, 'Infinity', '-0', '-infinity'),
            (4, 0, 0, TIMESTAMP '2023-01-01 00:00:00')
          ) AS v(id, d, f, ts)",
        &["d", "f", "ts"],
    );
    let cases: [(&str, &str, &[&str]); 12] = [
        ("?row ex:d ?d", "?d > 1", &["1"]),
        ("?row ex:d ?d", "?d < 1", &["4"]),
        ("?row ex:d ?d", "!(?d = 1.5)", &["4"]),
        ("?row ex:d ?d", "?d", &["1"]),
        (
            "?row ex:d ?d ; ex:d ?e",
            "!(?d != ?e)",
            &["1", "2", "3", "4"],
        ),
        ("?row ex:f ?f", "?f > 1", &["1"]),
        // NaN > 1 is false, not an error.
        ("?row ex:f ?f", "!(?f > 1)", &["2", "3", "4"]),
        ("?row ex:f ?f", "?f = 0", &["3", "4"]),
        ("?row ex:f ?f ; ex:f ?g", "?f = ?g", &["1", "3", "4"]),
        ("?row ex:f ?f", "?f", &["1"]),
        (
            "?row ex:ts ?t",
            "?t < \"2023-06-01T00:00:00\"^^xsd:dateTime",
            &["4"],
        ),
        (
            "?row ex:ts ?t",
            "!(?t = \"2024-01-01T00:00:00\"^^xsd:dateTime)",
            &["4"],
        ),
    ];
    for (patterns, filter, expected) in cases {
        assert_eq!(
            filtered_rows(&url, &mapping, patterns, filter).await,
            expected,
            "{filter}"
        );
    }
}

/// A constant compared as an RDF term is bound to NULL when no value of the
/// column's type renders as it. In a FILTER that fails the comparison
/// only: it is false under negation and does not empty the branch.
#[tokio::test]
async fn filter_constant_that_matches_no_value_is_false_not_an_error() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT * FROM (VALUES
            (1, 7, '00000000-0000-0000-0000-00000000000a'::uuid),
            (2, 8, '00000000-0000-0000-0000-00000000000b')
          ) AS v(id, n, u)",
        &["n", "u"],
    );
    let cases: [(&str, &str, &[&str]); 6] = [
        ("?row ex:n ?n", "sameTerm(?n, 7)", &["1"]),
        (
            "?row ex:n ?n",
            "!sameTerm(?n, \"07\"^^xsd:integer)",
            &["1", "2"],
        ),
        (
            "?row ex:n ?n",
            "sameTerm(?n, \"07\"^^xsd:integer) || ?n = 8",
            &["2"],
        ),
        (
            "?row ex:u ?u",
            "?u = \"00000000-0000-0000-0000-00000000000a\"",
            &["1"],
        ),
        (
            "?row ex:u ?u",
            "!(?u = \"00000000-0000-0000-0000-00000000000A\")",
            &["1", "2"],
        ),
        (
            "?row ex:u ?u",
            "?u > \"00000000-0000-0000-0000-00000000000a\"",
            &["2"],
        ),
    ];
    for (patterns, filter, expected) in cases {
        assert_eq!(
            filtered_rows(&url, &mapping, patterns, filter).await,
            expected,
            "{filter}"
        );
    }
}

/// Comparisons whose SQL would not match XPath are rejected when the column
/// types are known, not approximated.
#[tokio::test]
async fn filter_comparisons_without_exact_sql_are_rejected() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT 1 AS id, 0.1::real AS r, DATE '2024-01-01' AS d, 'x'::char(3) AS c",
        &["r", "d", "c"],
    );
    for (patterns, filter) in [
        ("?row ex:r ?r", "?r > 0"),
        ("?row ex:d ?d", "?d = \"infinity\""),
        ("?row ex:c ?c", "?c < \"y\""),
    ] {
        let query = format!(
            "PREFIX ex: <http://example.org/> SELECT ?row WHERE {{ {patterns} FILTER({filter}) }}"
        );
        let err = run(&url, Limits::default(), &plan(&mapping, &query))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Core(v12n_core::Error::UnsupportedFeature(_))),
            "{filter}: {err}"
        );
    }
}

/// REGEX follows XPath, not PostgreSQL's defaults: `.` does not match a
/// newline or carriage return, `$` is the end of the string, the `m` flag
/// does not find a line after a final newline, and `i` matches the case
/// variants XPath defines, independently of the database locale.
#[tokio::test]
async fn regex_follows_xpath_matching() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT * FROM (VALUES
            (1, 'Alice'), (2, 'ALI'), (3, 'al' || chr(305)), (4, 'a' || chr(10) || 'b'),
            (5, 'a' || chr(13) || 'b'), (6, 'ab' || chr(10)), (7, chr(8490) || 'elvin'),
            (8, 'x.y'), (9, 'x1y')
          ) AS v(id, s)",
        &["s"],
    );
    let cases: [(&str, &[&str]); 12] = [
        (r#"REGEX(?s, "^ali", "i")"#, &["1", "2", "3"]),
        (r#"REGEX(?s, "^ALI$")"#, &["2"]),
        (r#"REGEX(?s, "^a.b$")"#, &[]),
        (r#"REGEX(?s, "^a.b$", "s")"#, &["4", "5"]),
        (r#"REGEX(?s, "b$")"#, &["4", "5"]),
        (r#"REGEX(?s, "b$", "m")"#, &["4", "5", "6"]),
        (r#"REGEX(?s, "^$", "m")"#, &[]),
        (r#"REGEX(?s, "^b", "m")"#, &["4"]),
        (r#"REGEX(?s, "^Kelvin$", "i")"#, &["7"]),
        (r#"REGEX(?s, "x\\.y")"#, &["8"]),
        (r#"REGEX(?s, "x . y", "x")"#, &["8", "9"]),
        (r#"REGEX(?s, "\\s")"#, &["4", "5", "6"]),
    ];
    for (filter, expected) in cases {
        assert_eq!(
            filtered_rows(&url, &mapping, "?row ex:s ?s", filter).await,
            expected,
            "{filter}"
        );
    }
}

/// STR, LANG and DATATYPE of the terms rows give, including the infinite
/// timestamps that are plain literals.
#[tokio::test]
async fn str_lang_and_datatype_of_row_terms() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT * FROM (VALUES
            (1, TIMESTAMP '2024-01-01 00:00:00', 5, 'a b'),
            (2, 'infinity', 50, 'c')
          ) AS v(id, ts, n, s)",
        &["ts", "n", "s"],
    );
    let cases: [(&str, &str, &[&str]); 5] = [
        ("?row ex:ts ?t", "DATATYPE(?t) = xsd:string", &["2"]),
        (
            "?row ex:ts ?t",
            "DATATYPE(?t) = xsd:dateTime && LANG(?t) = \"\"",
            &["1"],
        ),
        // Strings, not numbers: "50" < "6".
        ("?row ex:n ?n", "STR(?n) < \"6\"", &["1", "2"]),
        (
            "?row ex:n ?n",
            "REGEX(STR(?n), \"^5\") && DATATYPE(?n) = xsd:integer",
            &["1", "2"],
        ),
        (
            "?row ex:s ?s",
            "REGEX(STR(?row), \"/row/1$\") && sameTerm(STR(?s), ?s)",
            &["1"],
        ),
    ];
    for (patterns, filter, expected) in cases {
        assert_eq!(
            filtered_rows(&url, &mapping, patterns, filter).await,
            expected,
            "{filter}"
        );
    }
}

/// Subjects from templates over columns of several types and from an IRI
/// column (#29): where the strings can be the same they are computed in
/// SQL, which must give exactly the IRIs the results are built as, so the
/// same triple counts once.
#[tokio::test]
async fn shapes_that_can_be_equal_are_compared_as_the_built_terms() {
    let Some(url) = database_url() else { return };
    let map = |name: &str, sql: &str, subject: &str| {
        format!(
            "<#{name}> rr:logicalTable [ rr:sqlQuery \"\"\"{sql}\"\"\" ] ;\n\
             rr:subjectMap [ {subject} ] ;\n\
             rr:predicateObjectMap [ rr:predicate ex:v ; rr:objectMap [ rr:column \"v\" ] ] .\n"
        )
    };
    let template = "rr:template \"http://example.org/row/{id}\"";
    let column = "rr:column \"iri\"";
    let mapping = format!(
        "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n@prefix ex: <http://example.org/> .\n{}{}{}{}",
        map("int", "SELECT 1 AS id, 'x'::text AS v", template),
        map(
            "numeric",
            "SELECT 1.50::numeric AS id, 'x'::text AS v",
            template
        ),
        map(
            "space",
            "SELECT 'a b'::text AS id, 'x'::text AS v",
            template
        ),
        map(
            "iri",
            "SELECT * FROM (VALUES ('http://example.org/row/1', 'x'), \
             ('http://example.org/row/1.5', 'x'), ('http://example.org/row/a%20b', 'x'), \
             ('http://example.org/row/a%2Db', 'x'), ('http://example.org/other', 'x')) AS r(iri, v)",
            column
        ),
    );
    let mut expected: Vec<String> = ["row/1", "row/1.5", "row/a%20b", "row/a%2Db", "other"]
        .iter()
        .map(|s| format!("?s=<http://example.org/{s}>"))
        .collect();
    expected.sort();
    for query in [
        "SELECT ?s WHERE { ?s <http://example.org/v> ?v }",
        "SELECT DISTINCT ?s WHERE { ?s <http://example.org/v> ?v }",
    ] {
        let (rows, _) = run(&url, Limits::default(), &plan(&mapping, query))
            .await
            .unwrap();
        assert_eq!(rows, expected, "{query}");
    }
    // A query-level UNION keeps each side's solutions: twice, or once
    // under DISTINCT.
    let both = "{ ?s <http://example.org/v> ?v } UNION { ?s <http://example.org/v> ?v }";
    let (rows, _) = run(
        &url,
        Limits::default(),
        &plan(&mapping, &format!("SELECT ?s WHERE {{ {both} }}")),
    )
    .await
    .unwrap();
    let mut twice: Vec<String> = expected.iter().chain(&expected).cloned().collect();
    twice.sort();
    assert_eq!(rows, twice);
    let (rows, _) = run(
        &url,
        Limits::default(),
        &plan(&mapping, &format!("SELECT DISTINCT ?s WHERE {{ {both} }}")),
    )
    .await
    .unwrap();
    assert_eq!(rows, expected);
}

/// An `OPTIONAL` part that two triples maps can match splits the branch:
/// the same triple from both counts once, left solutions that neither
/// extends stay once with the variable unbound, and a `UNION` inside the
/// part keeps each side's solutions.
#[tokio::test]
async fn optional_matched_by_several_triples_maps_splits_the_branch() {
    let Some(url) = database_url() else { return };
    let name =
        "rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column \"name\" ] ]";
    let mapping = views(&[
        (
            "orders",
            "SELECT * FROM (VALUES (1), (2), (3)) AS r(id)",
            "rr:predicateObjectMap [ rr:predicate ex:order ; rr:objectMap [ rr:column \"id\" ] ]",
        ),
        // Row 1 twice and also in the other view: one triple.
        (
            "people",
            "SELECT * FROM (VALUES (1, 'Ann'), (1, 'Ann'), (2, 'Bo')) AS r(id, name)",
            name,
        ),
        (
            "contractors",
            "SELECT * FROM (VALUES (1, 'Ann'), (2, 'Bea')) AS r(id, name)",
            name,
        ),
    ]);
    let query =
        |body: &str| format!("PREFIX ex: <http://example.org/>\nSELECT ?r ?n WHERE {{ {body} }}");
    let row = |id: u8, name: Option<&str>| match name {
        Some(name) => format!("?r=<http://example.org/row/{id}> ?n=\"{name}\""),
        None => format!("?r=<http://example.org/row/{id}>"),
    };
    let (rows, _) = run(
        &url,
        Limits::default(),
        &plan(
            &mapping,
            &query("?r ex:order ?o OPTIONAL { ?r ex:name ?n }"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            row(1, Some("Ann")),
            row(2, Some("Bea")),
            row(2, Some("Bo")),
            row(3, None)
        ]
    );
    // The filter decides per triples map; row 2 keeps Bo only, row 1 none.
    let (rows, _) = run(
        &url,
        Limits::default(),
        &plan(
            &mapping,
            &query(
                "?r ex:order ?o OPTIONAL { ?r ex:name ?n FILTER(?n != \"Ann\" && ?n != \"Bea\") }",
            ),
        ),
    )
    .await
    .unwrap();
    assert_eq!(rows, [row(1, None), row(2, Some("Bo")), row(3, None)]);
    // A union inside the part is a bag: Ann once from each side.
    let (rows, _) = run(
        &url,
        Limits::default(),
        &plan(
            &mapping,
            &query("?r ex:order ?o OPTIONAL { { ?r ex:name ?n } UNION { ?r ex:name ?n } }"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            row(1, Some("Ann")),
            row(1, Some("Ann")),
            row(2, Some("Bea")),
            row(2, Some("Bea")),
            row(2, Some("Bo")),
            row(2, Some("Bo")),
            row(3, None)
        ]
    );
}

/// One value per row, each from its own column, so that `?v` takes terms
/// of every kind; row 17 has none.
const KINDS: &str = r#"
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .
<#V> rr:logicalTable [ rr:sqlQuery """SELECT * FROM (VALUES
    (1, 10, NULL::numeric, NULL::float8, NULL, NULL::bool, NULL::timestamp, NULL::date, NULL, NULL, NULL::int),
    (2, 9, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
    (3, NULL, 9.5, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
    (4, NULL, NULL, 1e1, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
    (5, NULL, NULL, 'NaN', NULL, NULL, NULL, NULL, NULL, NULL, NULL),
    (6, NULL, NULL, NULL, 'a', NULL, NULL, NULL, NULL, NULL, NULL),
    (7, NULL, NULL, NULL, 'Z', NULL, NULL, NULL, NULL, NULL, NULL),
    (8, NULL, NULL, NULL, 'é', NULL, NULL, NULL, NULL, NULL, NULL),
    (9, NULL, NULL, NULL, NULL, true, NULL, NULL, NULL, NULL, NULL),
    (10, NULL, NULL, NULL, NULL, false, NULL, NULL, NULL, NULL, NULL),
    (11, NULL, NULL, NULL, NULL, NULL, '2024-01-01 00:00:00', NULL, NULL, NULL, NULL),
    (12, NULL, NULL, NULL, NULL, NULL, 'infinity', NULL, NULL, NULL, NULL),
    (13, NULL, NULL, NULL, NULL, NULL, NULL, '2024-01-01', NULL, NULL, NULL),
    (14, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'b', NULL, NULL),
    (15, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'http://example.org/Z', NULL),
    (16, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'http://example.org/a', NULL),
    (17, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 1)
  ) AS v(id, i, d, f, s, b, ts, dt, l, link, e)""" ] ;
  rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:i ; rr:objectMap [ rr:column "i" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:d ; rr:objectMap [ rr:column "d" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:f ; rr:objectMap [ rr:column "f" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:s ; rr:objectMap [ rr:column "s" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:b ; rr:objectMap [ rr:column "b" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:ts ; rr:objectMap [ rr:column "ts" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:dt ; rr:objectMap [ rr:column "dt" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:l ; rr:objectMap [ rr:column "l" ; rr:language "en" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:link ; rr:objectMap [ rr:column "link" ; rr:termType rr:IRI ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:e ; rr:objectMap [ rr:column "e" ] ] .
"#;

/// The row numbers of the solutions, in order.
async fn ordered_rows(url: &str, mapping: &str, body: &str, limits: Limits) -> (Vec<u32>, bool) {
    let query = format!(
        "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n{body}"
    );
    let (rows, truncated) = run_in_order(url, limits, &plan(mapping, &query))
        .await
        .unwrap_or_else(|e| panic!("{body}: {e}"));
    let ids = rows
        .iter()
        .map(|r| {
            let start = r.find("<http://example.org/row/").expect("a row IRI") + 24;
            let end = start + r[start..].find('>').expect("the end of the IRI");
            r[start..end].parse().expect("a row number")
        })
        .collect();
    (ids, truncated)
}

/// SPARQL 1.1 §15.1: no value, blank nodes, IRIs, then literals; IRIs and
/// strings by code point; numbers by value across datatypes, booleans,
/// dateTimes by `<`. Between kinds of literals SPARQL leaves the order
/// open; this engine puts numbers, booleans, dateTimes, strings,
/// language-tagged literals and then other datatypes. The infinite
/// timestamp is the plain literal "infinity", among the strings. 10 and
/// 1.0E1 are equal numbers: their order is open too.
#[tokio::test]
async fn order_by_follows_the_sparql_order_across_kinds_of_terms() {
    let Some(url) = database_url() else { return };
    let union = "SELECT ?row ?v WHERE { { ?row ex:i ?v } UNION { ?row ex:d ?v } \
                 UNION { ?row ex:f ?v } UNION { ?row ex:s ?v } UNION { ?row ex:b ?v } \
                 UNION { ?row ex:ts ?v } UNION { ?row ex:dt ?v } UNION { ?row ex:l ?v } \
                 UNION { ?row ex:link ?v } UNION { ?row ex:e ?x } }";
    let ascending = [17, 15, 16, 2, 3, 1, 4, 5, 10, 9, 11, 7, 6, 12, 8, 14, 13];
    let (rows, _) = ordered_rows(
        &url,
        KINDS,
        &format!("{union} ORDER BY ?v"),
        Limits::default(),
    )
    .await;
    assert_eq!(rows, ascending);
    // Descending is the reverse, unbound last.
    let (rows, _) = ordered_rows(
        &url,
        KINDS,
        &format!("{union} ORDER BY DESC(?v)"),
        Limits::default(),
    )
    .await;
    let mut descending = ascending;
    descending.reverse();
    assert_eq!(rows, descending);
}

/// Numbers compare by value with XPath's promotion: integers and decimals
/// exactly, and as doubles when the other is a double. NaN orders with
/// nothing; it comes after every number here, as in PostgreSQL.
#[tokio::test]
async fn order_by_compares_numbers_by_value_across_datatypes() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT * FROM (VALUES
            (1, -1, NULL::numeric, NULL::float8),
            (2, 3, NULL, NULL),
            (3, NULL, 0.10000000000000000001, NULL),
            (4, NULL, 0.1, NULL),
            (5, NULL, 2.5, NULL),
            (6, NULL, NULL, '-Infinity'),
            (7, NULL, NULL, 'Infinity'),
            (8, NULL, NULL, 'NaN'),
            (9, NULL, NULL, 0.5),
            (10, NULL, NULL, -0.0)
          ) AS v(id, i, d, f)",
        &["i", "d", "f"],
    );
    let exact = "SELECT ?row WHERE { { ?row ex:i ?v } UNION { ?row ex:d ?v } }";
    // Without the decimals that are equal to the double 0.1.
    let all = "SELECT ?row WHERE { { ?row ex:i ?v } UNION { ?row ex:d ?v } UNION { ?row ex:f ?v } \
               FILTER(!(?v > 0 && ?v < 0.2)) }";
    let cases: [(String, &[u32]); 3] = [
        // Two decimals that one double cannot tell apart.
        (format!("{exact} ORDER BY ?v"), &[1, 4, 3, 5, 2]),
        (format!("{exact} ORDER BY DESC(?v)"), &[2, 5, 3, 4, 1]),
        (format!("{all} ORDER BY ?v"), &[6, 1, 10, 9, 5, 2, 7, 8]),
    ];
    for (query, expected) in cases {
        let (rows, _) = ordered_rows(&url, &mapping, &query, Limits::default()).await;
        assert_eq!(rows, expected, "{query}");
    }
}

/// Unbound sorts first ascending and last descending; strings compare by
/// code point, not by the database collation; later comparators order
/// ties of earlier ones.
#[tokio::test]
async fn order_by_strings_by_code_point_with_several_comparators() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT * FROM (VALUES
            (1, 'b', 'é'), (2, 'a', 'Z'), (3, 'b', NULL), (4, 'a', 'z'),
            (5, 'b', '가'), (6, 'a', NULL), (7, 'b', 'a')
          ) AS v(id, g, s)",
        &["g", "s"],
    );
    let cases: [(&str, &[u32]); 3] = [
        (
            "SELECT ?row WHERE { ?row ex:g ?g OPTIONAL { ?row ex:s ?s } } ORDER BY ?s ?g",
            &[6, 3, 2, 7, 4, 1, 5],
        ),
        (
            "SELECT ?row WHERE { ?row ex:g ?g OPTIONAL { ?row ex:s ?s } } ORDER BY DESC(?s) ?g",
            &[5, 1, 4, 7, 2, 6, 3],
        ),
        (
            "SELECT ?row WHERE { ?row ex:g ?g OPTIONAL { ?row ex:s ?s } } \
             ORDER BY DESC(?g) (?s < \"b\") STR(?row)",
            &[3, 1, 5, 7, 6, 4, 2],
        ),
    ];
    for (query, expected) in cases {
        let (rows, _) = ordered_rows(&url, &mapping, query, Limits::default()).await;
        assert_eq!(rows, expected, "{query}");
    }
}

/// The infinite values of a timestamp column are the plain literals
/// "-infinity" and "infinity". Alone, the column sorts as it is: their
/// order against dateTimes is open, so they stay at its ends. With other
/// strings, they sort among them by code point.
#[tokio::test]
async fn order_by_puts_infinite_timestamps_among_strings_where_there_are_some() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT * FROM (VALUES
            (1, TIMESTAMP '2024-01-01 00:00:00', NULL),
            (2, 'infinity', NULL),
            (3, '-infinity', NULL),
            (4, '2023-01-01 00:00:00', NULL),
            (5, NULL, 'a')
          ) AS v(id, ts, s)",
        &["ts", "s"],
    );
    let alone = "SELECT ?row WHERE { ?row ex:ts ?v }";
    let mixed = "SELECT ?row WHERE { { ?row ex:ts ?v } UNION { ?row ex:s ?v } }";
    let cases: [(String, &[u32]); 4] = [
        (format!("{alone} ORDER BY ?v"), &[3, 4, 1, 2]),
        (format!("{alone} ORDER BY DESC(?v)"), &[2, 1, 4, 3]),
        (format!("{mixed} ORDER BY ?v"), &[4, 1, 3, 5, 2]),
        (format!("{mixed} ORDER BY DESC(?v)"), &[2, 5, 3, 1, 4]),
    ];
    for (query, expected) in cases {
        let (rows, _) = ordered_rows(&url, &mapping, &query, Limits::default()).await;
        assert_eq!(rows, expected, "{query}");
    }
}

/// OFFSET and LIMIT apply after ORDER BY and DISTINCT (SPARQL 1.1
/// §18.2.5). A query LIMIT at or below the row limit is never reported as
/// truncation; above it, the row limit still cuts and says so.
#[tokio::test]
async fn limit_and_offset_slice_after_order_and_distinct() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT * FROM (VALUES (1, 'x'), (2, 'y'), (3, 'x'), (4, 'z'), (5, 'y')) AS v(id, t)",
        &["t"],
    );
    let distinct = |modifiers: &str| {
        format!(
            "PREFIX ex: <http://example.org/>\n\
             SELECT DISTINCT ?t WHERE {{ ?row ex:t ?t }} ORDER BY DESC(?t) {modifiers}"
        )
    };
    for (modifiers, expected) in [
        ("", &["z", "y", "x"][..]),
        ("LIMIT 2", &["z", "y"]),
        ("OFFSET 1", &["y", "x"]),
        ("LIMIT 1 OFFSET 2", &["x"]),
        ("OFFSET 3", &[]),
        ("LIMIT 0", &[]),
    ] {
        let (rows, truncated) = run_in_order(
            &url,
            Limits::default(),
            &plan(&mapping, &distinct(modifiers)),
        )
        .await
        .unwrap();
        let expected: Vec<String> = expected.iter().map(|t| format!("?t=\"{t}\"")).collect();
        assert_eq!((rows, truncated), (expected, false), "{modifiers}");
    }

    let limits = Limits {
        row_limit: 2,
        ..Limits::default()
    };
    let body = "SELECT ?row WHERE { ?row ex:t ?t } ORDER BY ?row";
    for (modifiers, expected, truncated) in [
        ("LIMIT 2", &[1, 2][..], false),
        ("LIMIT 1", &[1], false),
        ("LIMIT 3", &[1, 2], true),
        ("OFFSET 3", &[4, 5], false),
        ("OFFSET 2", &[3, 4], true),
        ("OFFSET 2 LIMIT 2", &[3, 4], false),
    ] {
        let (rows, cut) =
            ordered_rows(&url, &mapping, &format!("{body} {modifiers}"), limits).await;
        assert_eq!((rows.as_slice(), cut), (expected, truncated), "{modifiers}");
    }
}

/// IRIs sort by their strings (SPARQL 1.1 §15.1), which SQL builds with
/// the IRI-safe encoding of R2RML §7.3: as they are for types whose
/// lexical forms only have `iunreserved` characters (integers with their
/// sign, uuids, booleans, decimals with NaN and the infinities, binaries)
/// and for text values made of them, encoded otherwise. All templates
/// share one prefix, so the keys of every type are compared with each
/// other.
#[tokio::test]
async fn iri_sort_keys_order_as_the_generated_iris() {
    let Some(url) = database_url() else { return };
    let rows = r"SELECT * FROM (VALUES
        (1, 0::int8, NULL::text, NULL::uuid, NULL::bool, NULL::numeric, NULL::bytea, NULL::int4),
        (2, -1, NULL, NULL, NULL, NULL, NULL, NULL),
        (3, -9, NULL, NULL, NULL, NULL, NULL, NULL),
        (4, -10, NULL, NULL, NULL, NULL, NULL, NULL),
        (5, 9, NULL, NULL, NULL, NULL, NULL, NULL),
        (6, 10, NULL, NULL, NULL, NULL, NULL, NULL),
        (7, 100, NULL, NULL, NULL, NULL, NULL, NULL),
        (8, 9223372036854775807, NULL, NULL, NULL, NULL, NULL, NULL),
        (9, -9223372036854775808, NULL, NULL, NULL, NULL, NULL, NULL),
        (10, NULL, 'a.b', NULL, NULL, NULL, NULL, NULL),
        (11, NULL, 'a/b', NULL, NULL, NULL, NULL, NULL),
        (12, NULL, 'a b', NULL, NULL, NULL, NULL, NULL),
        (13, NULL, '', NULL, NULL, NULL, NULL, NULL),
        (14, NULL, 'é', NULL, NULL, NULL, NULL, NULL),
        (15, NULL, '가%', NULL, NULL, NULL, NULL, NULL),
        (16, NULL, '-5', NULL, NULL, NULL, NULL, NULL),
        (17, NULL, '~x_y', NULL, NULL, NULL, NULL, NULL),
        (18, NULL, NULL, '00000000-0000-0000-0000-00000000000a', NULL, NULL, NULL, NULL),
        (19, NULL, NULL, 'FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF', NULL, NULL, NULL, NULL),
        (20, NULL, NULL, NULL, true, NULL, NULL, NULL),
        (21, NULL, NULL, NULL, false, NULL, NULL, NULL),
        (22, NULL, NULL, NULL, NULL, -12.50, NULL, NULL),
        (23, NULL, NULL, NULL, NULL, 3.00, NULL, NULL),
        (24, NULL, NULL, NULL, NULL, 0.000, NULL, NULL),
        (25, NULL, NULL, NULL, NULL, NULL, '\\x0a0bff', NULL),
        (26, -3, 'x/y', NULL, NULL, NULL, NULL, NULL),
        (27, -3, 'x.y', NULL, NULL, NULL, NULL, NULL),
        (28, NULL, NULL, NULL, NULL, NULL, NULL, -2147483648),
        (29, NULL, NULL, NULL, NULL, NULL, NULL, 2147483647),
        (30, NULL, NULL, NULL, NULL, 'NaN', NULL, NULL),
        (31, NULL, NULL, NULL, NULL, '-Infinity', NULL, NULL)
      ) AS v(id, i, t, u, b, d, x, i4)";
    let templates = [
        ("i", "{i}"),
        ("t", "{t}"),
        ("u", "{u}"),
        ("b", "{b}"),
        ("d", "{d}"),
        ("x", "{x}"),
        ("i4", "{i4}"),
        ("pair", "{i}/{t}"),
    ];
    let objects: String = templates
        .iter()
        .map(|(p, t)| {
            format!(
                "  rr:predicateObjectMap [ rr:predicate ex:{p} ; \
                 rr:objectMap [ rr:template \"http://example.org/v/{t}\" ] ] ;\n"
            )
        })
        .collect();
    let mapping = format!(
        "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
         @prefix ex: <http://example.org/> .\n\
         <#Values> rr:logicalTable [ rr:sqlQuery \"\"\"{rows}\"\"\" ] ;\n\
         {objects}  rr:subjectMap [ rr:template \"http://example.org/row/{{id}}\" ] .\n"
    );
    let all = templates
        .iter()
        .map(|(p, _)| format!("{{ ?row ex:{p} ?v }}"))
        .collect::<Vec<_>>()
        .join(" UNION ");
    let pair = "?row ex:pair ?v".to_owned();
    for (pattern, count) in [
        (all.clone(), 35),
        (pair, 2),
        ("?row ex:i ?v".to_owned(), 11),
    ] {
        for direction in ["", "DESC"] {
            let query = format!(
                "PREFIX ex: <http://example.org/> \
                 SELECT ?v WHERE {{ {pattern} }} ORDER BY {direction}(?v)"
            );
            let (rows, _) = run_in_order(&url, Limits::default(), &plan(&mapping, &query))
                .await
                .unwrap_or_else(|e| panic!("{query}: {e}"));
            assert_eq!(rows.len(), count, "{query}");
            let iris: Vec<&str> = rows
                .iter()
                .map(|r| {
                    r.strip_prefix("?v=<")
                        .and_then(|r| r.strip_suffix('>'))
                        .expect("an IRI")
                })
                .collect();
            // The engine builds the IRIs; Rust orders strings by code point.
            let mut expected = iris.clone();
            expected.sort();
            if direction == "DESC" {
                expected.reverse();
            }
            assert_eq!(iris, expected, "{query}");
        }
    }
    // The strings SQL builds are the IRIs: a FILTER compares them. `0` is
    // both an integer and a decimal; `a b` is encoded.
    for (iri, count) in [
        ("-9223372036854775808", 1),
        ("-10", 1),
        ("0", 2),
        ("a%2Fb", 1),
        ("a.b", 1),
        ("a%20b", 1),
        ("a b", 0),
        ("", 1),
        ("%C3%A9", 0),
        ("\u{e9}", 1),
        ("\u{ac00}%25", 1),
        ("00000000-0000-0000-0000-00000000000a", 1),
        ("ffffffff-ffff-ffff-ffff-ffffffffffff", 1),
        ("false", 1),
        ("-12.5", 1),
        ("3", 1),
        ("0A0BFF", 1),
        ("-3/x%2Fy", 1),
        ("-2147483648", 1),
        ("NaN", 1),
        ("-Infinity", 1),
    ] {
        let query = format!(
            "PREFIX ex: <http://example.org/> SELECT ?row ?v WHERE {{ {all} \
             FILTER(STR(?v) = \"http://example.org/v/{iri}\") }}"
        );
        let (rows, _) = run(&url, Limits::default(), &plan(&mapping, &query))
            .await
            .unwrap_or_else(|e| panic!("{query}: {e}"));
        assert_eq!(rows.len(), count, "{iri}: {rows:?}");
        assert!(
            rows.iter()
                .all(|r| r.ends_with(&format!("?v=<http://example.org/v/{iri}>"))),
            "{iri}: {rows:?}"
        );
    }
}

/// The order comes from the outermost query, through the row count, the
/// cast of columns without a decoder (`inet`) and the subqueries of
/// `UNION` and `DISTINCT`; the sort keys are not part of the solutions.
#[tokio::test]
async fn order_holds_through_every_wrapper_and_keys_stay_hidden() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT n AS id, (n * 37) % 101 AS k, ('10.0.0.' || n)::inet AS addr,
                 'g' || (n % 4) AS g
          FROM generate_series(1, 60) n",
        &["k", "addr", "g"],
    );
    let limits = |row_limit| Limits {
        row_limit,
        ..Limits::default()
    };
    // Every solution has exactly the projected variables.
    let query = "PREFIX ex: <http://example.org/>\n\
                 SELECT ?row ?addr WHERE { ?row ex:k ?k ; ex:addr ?addr } ORDER BY DESC(?k) LIMIT 8";
    let database = PostgresBackend::connect(&url, limits(5)).await.unwrap();
    let mut solutions = execute(&database, &plan(&mapping, query)).await.unwrap();
    assert_eq!(solutions.variables(), ["row", "addr"]);
    let mut keys = Vec::new();
    while let Some(solution) = solutions.try_next().await.unwrap() {
        assert_eq!(solution.values().len(), 2);
        let row = solution.values()[0].as_ref().unwrap().to_string();
        let id: i64 = row
            .trim_start_matches("<http://example.org/row/")
            .trim_end_matches('>')
            .parse()
            .unwrap();
        keys.push((id * 37) % 101);
    }
    assert!(solutions.is_truncated());
    assert_eq!(keys, [100, 99, 97, 96, 94]);

    let cases: [(&str, Limits, &[u32], bool); 4] = [
        // A query LIMIT under the row limit is never truncation.
        (
            "SELECT ?row WHERE { ?row ex:k ?k ; ex:addr ?a } ORDER BY ?k LIMIT 4",
            limits(5),
            &[41, 11, 52, 22],
            false,
        ),
        (
            "SELECT ?row WHERE { { ?row ex:k ?k FILTER(?k < 3) } UNION { ?row ex:g \"g0\" ; ex:k ?k } } \
             ORDER BY DESC(?k) ?row LIMIT 3",
            limits(3),
            &[60, 8, 16],
            false,
        ),
        (
            "SELECT DISTINCT ?row ?g WHERE { ?row ex:k ?k ; ex:g ?g } ORDER BY ?g DESC(?row) OFFSET 1",
            limits(3),
            &[60, 56, 52],
            true,
        ),
        (
            "SELECT ?row WHERE { ?row ex:k ?k } ORDER BY DESC(?k) OFFSET 58",
            limits(0),
            &[11, 41],
            false,
        ),
    ];
    for (body, limits, expected, truncated) in cases {
        let (rows, cut) = ordered_rows(&url, &mapping, body, limits).await;
        assert_eq!((rows.as_slice(), cut), (expected, truncated), "{body}");
    }
}

/// When one `SELECT` reads the column of the comparator, it sorts by the
/// column itself, so an index on it delivers the rows in order.
#[tokio::test]
async fn order_by_a_column_keeps_its_index() {
    let Some(url) = database_url() else { return };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            "DROP SCHEMA IF EXISTS v12n_order CASCADE;
             CREATE SCHEMA v12n_order;
             CREATE TABLE v12n_order.items (id int PRIMARY KEY, price int NOT NULL);
             INSERT INTO v12n_order.items SELECT n, (n * 7919) % 10007 FROM generate_series(1, 20000) n;
             CREATE INDEX items_price ON v12n_order.items (price);
             ANALYZE v12n_order.items;",
        )
        .await
        .unwrap();
    let mapping = "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
         <#M> rr:logicalTable [ rr:tableName \"v12n_order.items\" ] ;\n\
           rr:subjectMap [ rr:template \"http://example.org/item/{id}\" ] ;\n\
           rr:predicateObjectMap [ rr:predicate <http://example.org/price> ; rr:objectMap [ rr:column \"price\" ] ] .";
    let database = PostgresBackend::connect(&url, Limits::default())
        .await
        .unwrap();
    let mut plans = Vec::new();
    for direction in ["", "DESC"] {
        let plan = plan(
            mapping,
            &format!(
                "SELECT ?i ?p WHERE {{ ?i <http://example.org/price> ?p }} ORDER BY {direction}(?p) LIMIT 3"
            ),
        );
        let keys = database.table_keys(&plan).await.unwrap();
        let int = ColumnType {
            name: "int4".into(),
            sql_type: "\"pg_catalog\".\"int4\"".into(),
            union_class: "integer".into(),
            natural_datatype: Some("http://www.w3.org/2001/XMLSchema#integer".into()),
            group: "integer".into(),
            comparison: Comparison::Value,
            lexical: Some(Lexical::Text),
            lexical_unreserved: true,
            term_key: None,
            term_normal: None,
        };
        let table = LogicalTable::TableName("v12n_order.items".into());
        let types: ColumnTypes = [
            ((table.clone(), "id".to_owned()), int.clone()),
            ((table, "price".to_owned()), int),
        ]
        .into();
        let sql = plan
            .to_sql_with(
                &PostgresDialect,
                &RenderOptions {
                    keys: keys.as_ref(),
                    column_types: Some(&types),
                    ..RenderOptions::default()
                },
            )
            .unwrap();
        let (rows, _) = run_in_order(&url, Limits::default(), &plan).await.unwrap();
        plans.push((sql, rows));
    }
    client
        .batch_execute("SET enable_sort = off; SET enable_seqscan = off")
        .await
        .unwrap();
    for (rendered, rows) in &plans {
        let explain: Vec<String> = client
            .query(&format!("EXPLAIN {}", rendered.sql), &[&3i64])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert!(
            explain
                .iter()
                .any(|line| line.contains("Index Scan") && line.contains("items_price")),
            "{}\n{explain:#?}",
            rendered.sql
        );
        assert_eq!(rows.len(), 3);
    }
    assert!(plans[0].1[0].ends_with("?p=\"0\"^^<http://www.w3.org/2001/XMLSchema#integer>"));
    assert!(plans[1].1[0].ends_with("?p=\"10006\"^^<http://www.w3.org/2001/XMLSchema#integer>"));
    client
        .batch_execute("DROP SCHEMA v12n_order CASCADE")
        .await
        .unwrap();
}

/// What SQL cannot order the way SPARQL does is rejected.
#[tokio::test]
async fn order_by_without_an_exact_sql_order_is_rejected() {
    let Some(url) = database_url() else { return };
    let mapping = sql_query_mapping(
        r"SELECT * FROM (VALUES
            (1, TIMESTAMP '2024-01-01 00:00:00', TIMESTAMPTZ '2024-01-01 00:00:00Z', 1.5::real)
          ) AS v(id, ts, tz, r)",
        &["ts", "tz", "r"],
    );
    for (body, reason) in [
        (
            "SELECT ?row WHERE { { ?row ex:ts ?v } UNION { ?row ex:tz ?v } } ORDER BY ?v",
            "with and without a time zone",
        ),
        ("SELECT ?row WHERE { ?row ex:r ?v } ORDER BY ?v", "float4"),
    ] {
        let query = format!("PREFIX ex: <http://example.org/>\n{body}");
        let database = PostgresBackend::connect(&url, Limits::default())
            .await
            .unwrap();
        match execute(&database, &plan(&mapping, &query)).await {
            Err(Error::Core(v12n_core::Error::UnsupportedFeature(feature))) => {
                assert!(feature.contains(reason), "{feature}")
            }
            Err(e) => panic!("{body}: {e}"),
            Ok(_) => panic!("{body}: expected an unsupported-feature error"),
        }
    }
}

/// `-0 = 0` holds in PostgreSQL, while `-0.0E0` and `0.0E0` are different
/// `xsd:double` terms (#59): a zero constant matches only rows of its own
/// sign, through the same index as before.
#[tokio::test]
async fn zero_constants_match_only_rows_of_their_sign() {
    let Some(url) = database_url() else { return };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            "DROP SCHEMA IF EXISTS v12n_zero CASCADE;
             CREATE SCHEMA v12n_zero;
             CREATE TABLE v12n_zero.v (id int PRIMARY KEY, f float8 NOT NULL, r real NOT NULL);
             INSERT INTO v12n_zero.v VALUES (1, '-0', '-0'), (2, '0', '0'), (3, 1.5, 1.5);
             INSERT INTO v12n_zero.v SELECT n, n, n FROM generate_series(4, 5000) n;
             CREATE INDEX v_f ON v12n_zero.v (f);
             ANALYZE v12n_zero.v;",
        )
        .await
        .unwrap();
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        <#V> rr:logicalTable [ rr:tableName "v12n_zero.v" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:f ; rr:objectMap [ rr:column "f" ] ] ;
          rr:predicateObjectMap [ rr:predicate ex:r ; rr:objectMap [ rr:column "r" ; rr:datatype xsd:float ] ] ;
          rr:predicateObjectMap [ rr:predicate ex:zero ; rr:objectMap [ rr:constant "0.0E0"^^xsd:double ] ] .
    "#;
    let xsd = "http://www.w3.org/2001/XMLSchema#";
    let row = |id: u8| format!("?row=<http://example.org/row/{id}>");
    for (predicate, object, expected) in [
        ("f", format!("\"0.0E0\"^^<{xsd}double>"), vec![row(2)]),
        ("f", format!("\"-0.0E0\"^^<{xsd}double>"), vec![row(1)]),
        ("f", format!("\"1.5E0\"^^<{xsd}double>"), vec![row(3)]),
        ("r", format!("\"0.0E0\"^^<{xsd}float>"), vec![row(2)]),
        ("r", format!("\"-0.0E0\"^^<{xsd}float>"), vec![row(1)]),
    ] {
        assert_eq!(
            subjects(&url, mapping, predicate, &object).await,
            expected,
            "{predicate} {object}"
        );
    }
    // A constant of the mapping joined with a column compares the same way.
    let (rows, _) = run(
        &url,
        Limits::default(),
        &plan(
            mapping,
            "PREFIX ex: <http://example.org/> SELECT ?row WHERE { ?row ex:zero ?z ; ex:f ?z }",
        ),
    )
    .await
    .unwrap();
    assert_eq!(rows, [row(2)]);
    // sameTerm compares terms as a triple pattern does; `=` compares
    // numbers, and -0 = 0 there (XPath op:numeric-equal).
    for (filter, expected) in [
        ("sameTerm(?f, 0.0E0)", vec!["2"]),
        ("sameTerm(?f, -0.0E0)", vec!["1"]),
        ("!sameTerm(?f, 0.0E0) && ?f < 1", vec!["1"]),
        ("sameTerm(?r, \"1.5E0\"^^xsd:float)", vec!["3"]),
        ("?f = 0.0E0", vec!["1", "2"]),
    ] {
        assert_eq!(
            filtered_rows(&url, mapping, "?row ex:f ?f ; ex:r ?r", filter).await,
            expected,
            "{filter}"
        );
    }

    // The value comparison still finds the rows through the index; the
    // text of the value only filters them.
    let plan = plan(
        mapping,
        &format!("SELECT ?row WHERE {{ ?row <http://example.org/f> \"0.0E0\"^^<{xsd}double> }}"),
    );
    let sql = plan
        .to_sql_with(
            &PostgresDialect,
            &RenderOptions {
                keyed_params: &[(0, TermKey::Text)],
                ..RenderOptions::default()
            },
        )
        .unwrap()
        .sql;
    assert!(
        sql.contains(r#"(t0."f" = $1 AND t0."f"::text = $1::text)"#),
        "{sql}"
    );
    client
        .batch_execute("SET enable_seqscan = off")
        .await
        .unwrap();
    let explain: Vec<String> = client
        .query(&format!("EXPLAIN {sql}"), &[&0.0f64])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert!(
        explain
            .iter()
            .any(|line| line.contains("Index Cond: (f = ")),
        "{explain:#?}"
    );
    client
        .batch_execute("DROP SCHEMA v12n_zero CASCADE")
        .await
        .unwrap();
}

/// `time '24:00:00'` and `'00:00:00'` are different values in PostgreSQL
/// but the same `xsd:time` value, whose canonical form is `00:00:00`
/// (#37): both give that term, and constants, joins, removing duplicates
/// and sorting treat them as one, while a midnight constant still finds
/// its rows through the index.
#[tokio::test]
async fn midnight_as_24_00_is_the_term_00_00_00() {
    let Some(url) = database_url() else { return };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            "DROP SCHEMA IF EXISTS v12n_time CASCADE;
             CREATE SCHEMA v12n_time;
             CREATE TABLE v12n_time.shifts (id int PRIMARY KEY, starts time NOT NULL);
             INSERT INTO v12n_time.shifts VALUES (1, '00:00'), (2, '24:00'), (3, '12:30:00.5');
             CREATE TABLE v12n_time.alarms (at time PRIMARY KEY, name text NOT NULL);
             INSERT INTO v12n_time.alarms VALUES
               ('00:00', 'midnight'), ('24:00', 'end of day'), ('12:30:00.5', 'noon');
             CREATE TABLE v12n_time.breaks (id int PRIMARY KEY, at time NOT NULL);
             INSERT INTO v12n_time.breaks VALUES (1, '24:00');
             CREATE TABLE v12n_time.many (id int PRIMARY KEY, at time NOT NULL);
             INSERT INTO v12n_time.many
               SELECT n, time '00:00' + n * interval '1 second' FROM generate_series(1, 5000) n;
             CREATE INDEX many_at ON v12n_time.many (at);
             ANALYZE v12n_time.many;",
        )
        .await
        .unwrap();
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#Shift> rr:logicalTable [ rr:tableName "v12n_time.shifts" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:starts ; rr:objectMap [ rr:column "starts" ] ] .
        <#Alarm> rr:logicalTable [ rr:tableName "v12n_time.alarms" ] ;
          rr:subjectMap [ rr:template "http://example.org/alarm/{at}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:at ; rr:objectMap [ rr:column "at" ] ] ;
          rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "name" ] ] .
        <#Break> rr:logicalTable [ rr:tableName "v12n_time.breaks" ] ;
          rr:subjectMap [ rr:template "http://example.org/break/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:breakAt ; rr:objectMap [ rr:column "at" ] ] .
        <#Many> rr:logicalTable [ rr:tableName "v12n_time.many" ] ;
          rr:subjectMap [ rr:template "http://example.org/many/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:moment ; rr:objectMap [ rr:column "at" ] ] .
    "#;
    let time = |t: &str| format!("\"{t}\"^^<http://www.w3.org/2001/XMLSchema#time>");
    let row = |id: u8| format!("?row=<http://example.org/row/{id}>");
    let alarm = |t: &str| format!("<http://example.org/alarm/{}>", t.replace(':', "%3A"));
    let query = |body: &str| {
        plan(
            mapping,
            &format!(
                "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> {body}"
            ),
        )
    };
    let rows = |plan: SqlPlan| {
        let url = url.clone();
        async move { run(&url, Limits::default(), &plan).await.unwrap().0 }
    };

    assert_eq!(
        rows(query("SELECT ?row ?t WHERE { ?row ex:starts ?t }")).await,
        [
            format!("{} ?t={}", row(1), time("00:00:00")),
            format!("{} ?t={}", row(2), time("00:00:00")),
            format!("{} ?t={}", row(3), time("12:30:00.5")),
        ]
    );
    for (object, expected) in [
        (time("00:00:00"), vec![row(1), row(2)]),
        (time("24:00:00"), vec![]),
        (time("12:30:00.5"), vec![row(3)]),
    ] {
        assert_eq!(
            subjects(&url, mapping, "starts", &object).await,
            expected,
            "{object}"
        );
    }
    assert_eq!(
        filtered_rows(
            &url,
            mapping,
            "?row ex:starts ?t",
            "sameTerm(?t, \"00:00:00\"^^xsd:time)"
        )
        .await,
        ["1", "2"]
    );
    assert_eq!(
        rows(query(
            "SELECT ?row ?a WHERE { ?row ex:starts ?t . ?a ex:at ?t }"
        ))
        .await,
        [
            format!("{} ?a={}", row(1), alarm("00:00:00")),
            format!("{} ?a={}", row(2), alarm("00:00:00")),
            format!("{} ?a={}", row(3), alarm("12:30:00.5")),
        ]
    );
    // A break only at 24:00 joins the shift at 00:00 too.
    assert_eq!(
        rows(query(
            "SELECT ?row WHERE { ?row ex:starts ?t . ?b ex:breakAt ?t }"
        ))
        .await,
        [row(1), row(2)]
    );
    assert_eq!(
        rows(query("SELECT DISTINCT ?t WHERE { ?row ex:starts ?t }")).await,
        [
            format!("?t={}", time("00:00:00")),
            format!("?t={}", time("12:30:00.5")),
        ]
    );
    let (ordered, _) = run_in_order(
        &url,
        Limits::default(),
        &query("SELECT ?row WHERE { ?row ex:starts ?t } ORDER BY DESC(?t) DESC(?row)"),
    )
    .await
    .unwrap();
    assert_eq!(ordered, [row(3), row(2), row(1)]);
    // The two midnight alarms are one subject; the key on `at` proves
    // neither that their solutions differ nor that the two scans of the
    // table read one row.
    assert_eq!(
        rows(query("SELECT ?a ?n WHERE { ?a ex:at ?t ; ex:name ?n }")).await,
        [
            format!("?a={} ?n=\"end of day\"", alarm("00:00:00")),
            format!("?a={} ?n=\"midnight\"", alarm("00:00:00")),
            format!("?a={} ?n=\"noon\"", alarm("12:30:00.5")),
        ]
    );
    let database = PostgresBackend::connect(&url, Limits::default())
        .await
        .unwrap();
    let alarms = query("SELECT ?a ?n WHERE { ?a ex:at ?t ; ex:name ?n }");
    assert_eq!(
        database.table_keys(&alarms).await.unwrap(),
        Some(TableKeys::new())
    );
    assert_eq!(
        database.merge_keys(&alarms).await.unwrap(),
        Some(TableKeys::new())
    );

    // The column is compared with the constant first, so the index finds
    // the rows of both forms.
    let sql = query(&format!(
        "SELECT ?m WHERE {{ ?m ex:moment {} }}",
        time("00:00:00")
    ))
    .to_sql_with(
        &PostgresDialect,
        &RenderOptions {
            normal_params: &[(0, TermNormal::Time)],
            ..RenderOptions::default()
        },
    )
    .unwrap()
    .sql;
    client
        .batch_execute("SET enable_seqscan = off")
        .await
        .unwrap();
    let explain: Vec<String> = client
        .query(
            &format!("EXPLAIN {}", sql.replace("$1", "time '00:00:00'")),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert!(
        explain
            .iter()
            .any(|line| line.contains("Index Cond: (at = ")),
        "{explain:#?}"
    );
    client
        .batch_execute("DROP SCHEMA v12n_time CASCADE")
        .await
        .unwrap();
}

/// Joins compare floating point columns by their text output, which tells
/// `-0` from `0`, and removing duplicates compares it too.
#[tokio::test]
async fn joins_and_duplicates_tell_minus_zero_from_zero() {
    let Some(url) = database_url() else { return };
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#A> rr:logicalTable [ rr:sqlQuery """SELECT * FROM (VALUES (1, '-0'::float8), (2, '0'::float8), (2, '-0'::float8)) v(id, f)""" ] ;
          rr:subjectMap [ rr:template "http://example.org/a/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:f ; rr:objectMap [ rr:column "f" ] ] .
        <#B> rr:logicalTable [ rr:sqlQuery """SELECT * FROM (VALUES (1, '0'::float8)) v(id, g)""" ] ;
          rr:subjectMap [ rr:template "http://example.org/b/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:g ; rr:objectMap [ rr:column "g" ] ] ;
          rr:predicateObjectMap [ rr:predicate ex:i ; rr:objectMap [ rr:template "http://example.org/v/{g}" ] ] .
        <#C> rr:logicalTable [ rr:sqlQuery """SELECT * FROM (VALUES (1, '-0'::float8)) v(id, g)""" ] ;
          rr:subjectMap [ rr:template "http://example.org/c/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:i ; rr:objectMap [ rr:template "http://example.org/v/{g}" ] ] .
    "#;
    let rows = |query: &str| {
        let plan = plan(
            mapping,
            &format!("PREFIX ex: <http://example.org/> {query}"),
        );
        let url = url.clone();
        async move { run(&url, Limits::default(), &plan).await.unwrap().0 }
    };
    let double = |v: &str| format!("\"{v}\"^^<http://www.w3.org/2001/XMLSchema#double>");
    assert_eq!(
        rows("SELECT ?a ?b WHERE { ?a ex:f ?x . ?b ex:g ?x }").await,
        ["?a=<http://example.org/a/2> ?b=<http://example.org/b/1>"]
    );
    assert_eq!(
        rows("SELECT DISTINCT ?x WHERE { ?a ex:f ?x }").await,
        [
            format!("?x={}", double("-0.0E0")),
            format!("?x={}", double("0.0E0"))
        ]
    );
    // Row 2 has two triples, so the hidden ?x keeps it twice.
    assert_eq!(
        rows("SELECT ?a WHERE { ?a ex:f ?x }").await,
        [
            "?a=<http://example.org/a/1>",
            "?a=<http://example.org/a/2>",
            "?a=<http://example.org/a/2>"
        ]
    );
    assert_eq!(
        rows("SELECT DISTINCT ?x WHERE { { ?a ex:f ?x } UNION { ?b ex:g ?x } }").await,
        [
            format!("?x={}", double("-0.0E0")),
            format!("?x={}", double("0.0E0"))
        ]
    );
    // IRIs from templates over the two zeros are two IRIs.
    assert_eq!(
        rows("SELECT DISTINCT ?i WHERE { ?s ex:i ?i }").await,
        [
            "?i=<http://example.org/v/-0.0E0>",
            "?i=<http://example.org/v/0.0E0>"
        ]
    );
}

/// `bpchar` values keep their padding, and so do their literals, while
/// its `=` ignores trailing spaces: every comparison also compares the
/// lengths.
#[tokio::test]
async fn padded_strings_compare_as_the_terms_they_give() {
    let Some(url) = database_url() else { return };
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#A> rr:logicalTable [ rr:sqlQuery """SELECT 1 AS id, 'a'::char(3) AS c3, 'a'::char(5) AS c5""" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:c ; rr:objectMap [ rr:column "c3" ] ] ;
          rr:predicateObjectMap [ rr:predicate ex:d ; rr:objectMap [ rr:column "c5" ] ] .
        <#B> rr:logicalTable [ rr:sqlQuery """SELECT 2 AS id, 'a'::char(5) AS c5""" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:c ; rr:objectMap [ rr:column "c5" ] ] .
    "#;
    let rows = |query: &str| {
        let plan = plan(
            mapping,
            &format!("PREFIX ex: <http://example.org/> {query}"),
        );
        let url = url.clone();
        async move { run(&url, Limits::default(), &plan).await.unwrap().0 }
    };
    let row = |id: u8| format!("?s=<http://example.org/row/{id}>");
    assert_eq!(rows("SELECT ?s WHERE { ?s ex:c \"a  \" }").await, [row(1)]);
    assert_eq!(
        rows("SELECT ?s WHERE { ?s ex:c \"a    \" }").await,
        [row(2)]
    );
    assert!(rows("SELECT ?s WHERE { ?s ex:c \"a\" }").await.is_empty());
    assert!(
        rows("SELECT ?s WHERE { ?s ex:c ?x ; ex:d ?x }")
            .await
            .is_empty()
    );
    assert_eq!(
        rows("SELECT ?s WHERE { ?s ex:c ?x FILTER(?x = \"a  \") }").await,
        [row(1)]
    );
    assert_eq!(
        rows("SELECT DISTINCT ?x WHERE { ?s ex:c ?x }").await,
        ["?x=\"a    \"", "?x=\"a  \""]
    );
}

/// Language tags compare case-insensitively (RDF 1.1): the mapping's
/// tags are kept in lower case, so tags that differ only in case give
/// one term.
#[tokio::test]
async fn language_tags_that_differ_in_case_are_one_term() {
    let Some(url) = database_url() else { return };
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#A> rr:logicalTable [ rr:sqlQuery "SELECT 1 AS id, 'chat'::text AS t" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:label ;
            rr:objectMap [ rr:column "t" ; rr:language "EN-us" ], [ rr:column "t" ; rr:language "en-US" ] ] ;
          rr:predicateObjectMap [ rr:predicate ex:other ; rr:objectMap [ rr:column "t" ; rr:language "en-us" ] ] .
    "#;
    let rows = |query: &str| {
        let plan = plan(
            mapping,
            &format!("PREFIX ex: <http://example.org/> {query}"),
        );
        let url = url.clone();
        async move { run(&url, Limits::default(), &plan).await.unwrap().0 }
    };
    assert_eq!(
        rows("SELECT ?x WHERE { ?s ex:label ?x }").await,
        ["?x=\"chat\"@en-us"]
    );
    assert_eq!(
        rows("SELECT DISTINCT ?x WHERE { { ?s ex:label ?x } UNION { ?s ex:other ?x } }").await,
        ["?x=\"chat\"@en-us"]
    );
    assert_eq!(
        rows("SELECT ?s WHERE { ?s ex:label \"chat\"@EN-US ; ex:other ?x FILTER(LANG(?x) = \"en-us\") }")
            .await,
        ["?s=<http://example.org/row/1>"]
    );
}

/// A query whose projected variables read no column, only constants of
/// the mapping, selects a constant in their place; the row count after it
/// is still found.
#[tokio::test]
async fn constant_terms_alone_are_projected() {
    let Some(url) = database_url() else { return };
    let mapping = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix ex: <http://example.org/> .
        <#A> rr:logicalTable [ rr:sqlQuery "SELECT * FROM (VALUES (1), (2)) v(id)" ] ;
          rr:subjectMap [ rr:template "http://example.org/row/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:label ; rr:objectMap [ rr:constant "Chat"@EN ] ] .
    "#;
    for (query, expected) in [
        ("SELECT ?x WHERE { ?s ex:label ?x }", 2),
        ("SELECT DISTINCT ?x WHERE { ?s ex:label ?x }", 1),
        (
            "SELECT * WHERE { <http://example.org/row/1> ex:label \"Chat\"@en }",
            1,
        ),
    ] {
        let plan = plan(
            mapping,
            &format!("PREFIX ex: <http://example.org/> {query}"),
        );
        let (rows, truncated) = run(&url, Limits::default(), &plan)
            .await
            .unwrap_or_else(|e| panic!("{query}: {e}"));
        assert_eq!(rows.len(), expected, "{query}: {rows:?}");
        assert!(!truncated);
        if expected == 2 {
            assert_eq!(rows[0], "?x=\"Chat\"@en");
        }
    }
}

/// Keys that merge two scans of a table identify a row by SQL `=`: not
/// `NULL`, the index's collation and default operator class as the
/// column's, and a type whose `=` needs no cast or term key. The keys for
/// removing duplicates stay as they were.
#[tokio::test]
async fn merge_keys_identify_one_row_by_equality() {
    let Some(url) = database_url() else { return };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            r#"DROP SCHEMA IF EXISTS v12n_merge CASCADE;
            CREATE SCHEMA v12n_merge;
            CREATE TABLE v12n_merge.pk (id int PRIMARY KEY, v text, w text);
            CREATE TABLE v12n_merge.composite (a int, b bigint, v text, w text, PRIMARY KEY (a, b));
            CREATE TABLE v12n_merge.unique_not_null (id int, v text NOT NULL UNIQUE, w text);
            CREATE TABLE v12n_merge.nullable (id int, v text UNIQUE, w text);
            CREATE TABLE v12n_merge.expression (id int NOT NULL, v text, w text);
            CREATE UNIQUE INDEX ON v12n_merge.expression (id, lower(v));
            CREATE TABLE v12n_merge.double (id float8 PRIMARY KEY, v text, w text);
            CREATE TABLE v12n_merge.padded (id char(3) PRIMARY KEY, v text, w text);
            CREATE TABLE v12n_merge.opclass (id int, v text NOT NULL, w text);
            CREATE UNIQUE INDEX ON v12n_merge.opclass (v text_pattern_ops);
            CREATE TABLE v12n_merge.collation (id int, v text COLLATE "C" NOT NULL, w text);
            CREATE UNIQUE INDEX ON v12n_merge.collation (v COLLATE "POSIX");
            CREATE TABLE v12n_merge.decimal (id numeric PRIMARY KEY, v text, w text);
            CREATE TABLE v12n_merge.clock (id time PRIMARY KEY, v text, w text);
            CREATE TABLE v12n_merge.parent (id int PRIMARY KEY, v text, w text);
            CREATE TABLE v12n_merge.child () INHERITS (v12n_merge.parent);
            CREATE TABLE v12n_merge.partitioned (id int PRIMARY KEY, v text, w text) PARTITION BY RANGE (id);
            CREATE TABLE v12n_merge.partition PARTITION OF v12n_merge.partitioned FOR VALUES FROM (0) TO (10);"#,
        )
        .await
        .unwrap();
    let database = PostgresBackend::connect(&url, Limits::default())
        .await
        .unwrap();
    let mut found = Vec::new();
    for table in [
        "pk",
        "composite",
        "unique_not_null",
        "nullable",
        "expression",
        "double",
        "padded",
        "opclass",
        "collation",
        "decimal",
        "clock",
        "parent",
        "partitioned",
    ] {
        let mapping = format!(
            "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
             <#M> rr:logicalTable [ rr:tableName \"v12n_merge.{table}\" ] ;\n\
               rr:subjectMap [ rr:template \"http://example.org/{{id}}\" ] ;\n\
               rr:predicateObjectMap [ rr:predicate <http://example.org/v> ; rr:objectMap [ rr:column \"v\" ] ] ;\n\
               rr:predicateObjectMap [ rr:predicate <http://example.org/w> ; rr:objectMap [ rr:column \"w\" ] ] ."
        );
        let plan = plan(
            &mapping,
            "SELECT ?s WHERE { ?s <http://example.org/v> ?v ; <http://example.org/w> ?w }",
        );
        let mut keys: Vec<Vec<String>> = database
            .merge_keys(&plan)
            .await
            .unwrap()
            .expect("the table is scanned twice")
            .into_values()
            .flatten()
            .collect();
        keys.sort();
        found.push((table, keys));
    }
    client
        .batch_execute("DROP SCHEMA v12n_merge CASCADE")
        .await
        .unwrap();
    let k = |columns: &[&str]| vec![columns.iter().map(|c| c.to_string()).collect::<Vec<_>>()];
    let none: Vec<Vec<String>> = Vec::new();
    assert_eq!(
        found,
        [
            ("pk", k(&["id"])),
            ("composite", k(&["a", "b"])),
            ("unique_not_null", k(&["v"])),
            ("nullable", none.clone()),
            ("expression", none.clone()),
            ("double", none.clone()),
            ("padded", none.clone()),
            ("opclass", none.clone()),
            ("collation", none.clone()),
            ("decimal", k(&["id"])),
            ("clock", none.clone()),
            ("parent", none.clone()),
            ("partitioned", k(&["id"])),
        ]
    );
}

const SELF_JOIN: &str = r#"@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .
<#Asset> rr:logicalTable [ rr:tableName "v12n_self.assets" ] ;
  rr:subjectMap [ rr:template "http://example.org/asset/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:hostname ; rr:objectMap [ rr:column "hostname" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:criticality ; rr:objectMap [ rr:column "criticality" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:site ; rr:objectMap [ rr:column "site" ] ] .
<#Badge> rr:logicalTable [ rr:tableName "v12n_self.badges" ] ;
  rr:subjectMap [ rr:template "http://example.org/badge/{code}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:label ; rr:objectMap [ rr:column "label" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:holder ; rr:objectMap [ rr:column "holder" ] ] .
<#Group> rr:logicalTable [ rr:tableName "v12n_self.groups" ] ;
  rr:subjectMap [ rr:template "http://example.org/group/{grp}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:x ; rr:objectMap [ rr:column "x" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:y ; rr:objectMap [ rr:column "y" ] ] .
"#;

/// Merged scans return the same solutions, and scans that no key pins to
/// one row stay: a nullable unique column (a row without a code has no
/// subject), a unique index over an expression (two rows of one group),
/// and the two sides of `OPTIONAL` (an asset with a criticality but no
/// site binds neither).
#[tokio::test]
async fn merged_scans_give_the_same_solutions() {
    let Some(url) = database_url() else { return };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            r#"DROP SCHEMA IF EXISTS v12n_self CASCADE;
            CREATE SCHEMA v12n_self;
            CREATE TABLE v12n_self.assets (id int PRIMARY KEY, hostname text NOT NULL, criticality text, site text);
            INSERT INTO v12n_self.assets VALUES (1, 'a', 'High', 'x'), (2, 'b', 'High', NULL), (3, 'c', NULL, 'y');
            CREATE TABLE v12n_self.badges (code text UNIQUE, label text, holder text);
            INSERT INTO v12n_self.badges VALUES ('k1', 'one', 'kim'), (NULL, 'none', 'lee');
            CREATE TABLE v12n_self.groups (grp int NOT NULL, name text NOT NULL, x text, y text);
            CREATE UNIQUE INDEX ON v12n_self.groups (grp, lower(name));
            INSERT INTO v12n_self.groups VALUES (1, 'a', 'x1', 'y1'), (1, 'b', 'x2', 'y2');"#,
        )
        .await
        .unwrap();
    let database = PostgresBackend::connect(&url, Limits::default())
        .await
        .unwrap();
    let mut results = Vec::new();
    for (body, scans) in [
        (r#"?a ex:criticality "High" ; ex:hostname ?h"#, 1),
        (
            "<http://example.org/asset/2> ex:hostname ?h ; ex:criticality ?c",
            1,
        ),
        (
            "?a ex:hostname ?h OPTIONAL { ?a ex:criticality ?c ; ex:site ?s }",
            2,
        ),
        ("?b ex:label ?l ; ex:holder ?h", 2),
        ("?g ex:x ?x ; ex:y ?y", 2),
    ] {
        let plan = plan(
            SELF_JOIN,
            &format!("PREFIX ex: <http://example.org/>\nSELECT * WHERE {{ {body} }}"),
        );
        let keys = database.merge_keys(&plan).await.unwrap().unwrap();
        let merged = v12n_core::optimize::eliminate_self_joins(&plan, &keys);
        assert_eq!(
            merged.as_ref().unwrap_or(&plan).branches[0].from.len(),
            scans,
            "{body}"
        );
        let (rows, _) = run(&url, Limits::default(), &plan).await.unwrap();
        results.push(rows);
    }
    client
        .batch_execute("DROP SCHEMA v12n_self CASCADE")
        .await
        .unwrap();
    let asset = |id: u32| format!("<http://example.org/asset/{id}>");
    assert_eq!(
        results,
        [
            vec![
                format!("?a={} ?h=\"a\"", asset(1)),
                format!("?a={} ?h=\"b\"", asset(2)),
            ],
            vec!["?c=\"High\" ?h=\"b\"".to_owned()],
            vec![
                format!("?a={} ?c=\"High\" ?h=\"a\" ?s=\"x\"", asset(1)),
                format!("?a={} ?h=\"b\"", asset(2)),
                format!("?a={} ?h=\"c\"", asset(3)),
            ],
            vec!["?b=<http://example.org/badge/k1> ?h=\"kim\" ?l=\"one\"".to_owned()],
            vec![
                "?g=<http://example.org/group/1> ?x=\"x1\" ?y=\"y1\"".to_owned(),
                "?g=<http://example.org/group/1> ?x=\"x1\" ?y=\"y2\"".to_owned(),
                "?g=<http://example.org/group/1> ?x=\"x2\" ?y=\"y1\"".to_owned(),
                "?g=<http://example.org/group/1> ?x=\"x2\" ?y=\"y2\"".to_owned(),
            ],
        ]
    );
}

/// Round trips between a client and PostgreSQL, counted by a proxy in
/// front of the database ([`wire_counter`]) as `bench/bench.py` counts
/// them: requests (frontend messages up to a Sync, or a simple Query) that
/// the client sends while none of its earlier requests is waiting for its
/// ReadyForQuery. Pipelined requests count once. A request of Close
/// messages only, which tokio-postgres sends when a statement is dropped
/// and never waits for, does not count.
#[derive(Default)]
struct Wire {
    round_trips: usize,
    /// Whether the client waits for each request still without its
    /// ReadyForQuery.
    pending: VecDeque<bool>,
    /// Message types of the request being sent.
    sending: Vec<u8>,
}

impl Wire {
    fn request(&mut self, kinds: &[u8]) {
        let waits = kinds.iter().any(|k| !matches!(k, b'C' | b'S'));
        if waits && !self.pending.iter().any(|&w| w) {
            self.round_trips += 1;
        }
        self.pending.push_back(waits);
    }
}

/// A connection string for the database at `url` through a proxy that
/// counts round trips.
async fn wire_counter(url: &str) -> (String, Arc<Mutex<Wire>>) {
    let config: tokio_postgres::Config = url.parse().unwrap();
    let Host::Tcp(host) = &config.get_hosts()[0] else {
        panic!("the test database is reached over TCP");
    };
    let target = (
        host.clone(),
        config.get_ports().first().copied().unwrap_or(5432),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let wire = Arc::new(Mutex::new(Wire::default()));
    let shared = Arc::clone(&wire);
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let server = TcpStream::connect(target.clone()).await.unwrap();
            client.set_nodelay(true).unwrap();
            server.set_nodelay(true).unwrap();
            let (client_read, client_write) = client.into_split();
            let (server_read, server_write) = server.into_split();
            tokio::spawn(pipe(client_read, server_write, Arc::clone(&shared), true));
            tokio::spawn(pipe(server_read, client_write, Arc::clone(&shared), false));
        }
    });
    let password = config
        .get_password()
        .map(|p| String::from_utf8(p.to_vec()).unwrap())
        .unwrap_or_default();
    let proxied = format!(
        "host=127.0.0.1 port={port} user={} password={password} dbname={} sslmode=disable",
        config.get_user().unwrap(),
        config.get_dbname().unwrap(),
    );
    (proxied, wire)
}

/// Forwards one direction. A client request is counted before it is
/// forwarded and a ReadyForQuery is taken off before it is forwarded, so
/// a request sent after an answer arrived always counts.
async fn pipe(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    wire: Arc<Mutex<Wire>>,
    from_client: bool,
) {
    let mut buf = Vec::new();
    let mut chunk = vec![0; 1 << 16];
    // The startup message has no type byte; with `sslmode=disable` it is
    // the client's first.
    let mut startup = from_client;
    loop {
        let n = match from.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        {
            let mut wire = wire.lock().unwrap();
            loop {
                let header = usize::from(!startup);
                let Some(length) = buf
                    .get(header..header + 4)
                    .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize)
                else {
                    break;
                };
                if buf.len() < header + length {
                    break;
                }
                let kind = buf[0];
                buf.drain(..header + length);
                if startup {
                    startup = false;
                    wire.request(b"startup");
                } else if !from_client {
                    if kind == b'Z' {
                        wire.pending.pop_front();
                    }
                } else if matches!(kind, b'S' | b'Q') {
                    let mut kinds = std::mem::take(&mut wire.sending);
                    kinds.push(kind);
                    wire.request(&kinds);
                } else if kind != b'X' {
                    wire.sending.push(kind);
                }
            }
        }
        if to.write_all(&chunk[..n]).await.is_err() {
            return;
        }
    }
}

/// The solutions of `plan` on `engine`, in the order they arrive.
async fn solutions(engine: &Engine<PostgresBackend>, plan: &SqlPlan) -> Vec<String> {
    let mut solutions = engine.execute(plan).await.unwrap();
    let mut rows = Vec::new();
    while let Some(solution) = solutions.try_next().await.unwrap() {
        rows.push(format!("{:?}", solution.values()));
    }
    rows
}

/// An engine for `mapping` on `database`, with the catalog of the mapping
/// read, as `v12n serve` starts.
async fn engine(mapping: &str, database: PostgresBackend) -> Engine<PostgresBackend> {
    let planner = Planner::new(Mapping::from_turtle(mapping).unwrap());
    let engine = Engine::new(Arc::new(planner), Arc::default(), database);
    engine.load_catalog().await.unwrap();
    engine
}

/// The fingerprint check and the statement go to the database together,
/// so a query takes two round trips: that and running the statement. A
/// query whose constants cannot match skips running the statement.
#[tokio::test]
async fn queries_take_two_round_trips() {
    let Some(url) = database_url() else { return };
    let (proxied, wire) = wire_counter(&url).await;
    let database = PostgresBackend::connect(&proxied, Limits::default())
        .await
        .unwrap();
    let engine = engine(SHOP, database).await;
    let name = |id: &str| {
        format!(
            "SELECT ?n WHERE {{ <http://example.org/customer/{id}> <http://example.org/name> ?n }}"
        )
    };
    for (query, rows, round_trips) in [
        // Keys show that no duplicates need removing.
        (name("7"), 1, 2),
        (VIP_ORDERS.to_owned(), 3, 2),
        // No keys to read: the column types tell how to remove duplicates.
        (
            "SELECT DISTINCT ?t WHERE { ?c <http://example.org/tier> ?t }".to_owned(),
            2,
            2,
        ),
        (
            "SELECT ?x WHERE { { ?x <http://example.org/name> ?v } UNION { ?x <http://example.org/amount> ?v } }"
                .to_owned(),
            7,
            2,
        ),
        // A constant that cannot match.
        (name("007"), 0, 1),
    ] {
        let plan = plan(SHOP, &query);
        let mut results = Vec::new();
        for _ in 0..2 {
            let before = wire.lock().unwrap().round_trips;
            let found = solutions(&engine, &plan).await;
            let taken = wire.lock().unwrap().round_trips - before;
            assert_eq!((found.len(), taken), (rows, round_trips), "{query}");
            results.push(found);
        }
        assert_eq!(results[0], results[1], "{query}");
    }
}

/// A statement whose parameter types call for rendering it again takes a
/// round trip more the first time; the connection then prepares the
/// retyped statement together with the first one.
#[tokio::test]
async fn retyped_statements_are_prepared_in_the_same_round_trip_next_time() {
    let Some(url) = database_url() else { return };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            "DROP SCHEMA IF EXISTS v12n_retyped CASCADE;
             CREATE SCHEMA v12n_retyped;
             CREATE TABLE v12n_retyped.items (id int PRIMARY KEY, code char(3) NOT NULL);
             INSERT INTO v12n_retyped.items VALUES (1, 'a'), (2, 'b');",
        )
        .await
        .unwrap();
    let mapping = "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
         <#M> rr:logicalTable [ rr:tableName \"v12n_retyped.items\" ] ;\n\
           rr:subjectMap [ rr:template \"http://example.org/item/{id}\" ] ;\n\
           rr:predicateObjectMap [ rr:predicate <http://example.org/code> ; \
           rr:objectMap [ rr:column \"code\" ] ] .";
    let plan = plan(
        mapping,
        "SELECT ?i WHERE { ?i <http://example.org/code> \"a  \" }",
    );
    let (proxied, wire) = wire_counter(&url).await;
    let database = PostgresBackend::connect(&proxied, Limits::default())
        .await
        .unwrap();
    let engine = engine(mapping, database).await;
    let mut taken = Vec::new();
    for _ in 0..2 {
        let before = wire.lock().unwrap().round_trips;
        assert_eq!(solutions(&engine, &plan).await.len(), 1);
        taken.push(wire.lock().unwrap().round_trips - before);
    }
    client
        .batch_execute("DROP SCHEMA v12n_retyped CASCADE")
        .await
        .unwrap();
    assert_eq!(taken, [3, 2]);
}

/// A snapshot is never trusted once the database differs from it: after
/// the key that let the query keep every row is dropped and a row is
/// duplicated, the fingerprint check fails, the tables are read again and
/// the statement that removes duplicates runs, and the row counts once.
#[tokio::test]
async fn a_query_does_not_trust_a_dropped_key() {
    let Some(url) = database_url() else { return };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            "DROP SCHEMA IF EXISTS v12n_known CASCADE;
             CREATE SCHEMA v12n_known;
             CREATE TABLE v12n_known.items (id int PRIMARY KEY, tag text NOT NULL);
             INSERT INTO v12n_known.items VALUES (1, 'a'), (2, 'b');",
        )
        .await
        .unwrap();
    let mapping = "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
         <#M> rr:logicalTable [ rr:tableName \"v12n_known.items\" ] ;\n\
           rr:subjectMap [ rr:template \"http://example.org/item/{id}\" ] ;\n\
           rr:predicateObjectMap [ rr:predicate <http://example.org/tag> ; \
           rr:objectMap [ rr:column \"tag\" ] ] .";
    let plan = plan(
        mapping,
        "SELECT ?i ?t WHERE { ?i <http://example.org/tag> ?t }",
    );
    let (proxied, wire) = wire_counter(&url).await;
    let database = PostgresBackend::connect(&proxied, Limits::default())
        .await
        .unwrap();
    let engine = engine(mapping, database).await;
    let round_trips = || wire.lock().unwrap().round_trips;
    let before = round_trips();
    let first = solutions(&engine, &plan).await;
    assert_eq!(round_trips() - before, 2);

    client
        .batch_execute(
            "ALTER TABLE v12n_known.items DROP CONSTRAINT items_pkey;
             INSERT INTO v12n_known.items VALUES (1, 'a');",
        )
        .await
        .unwrap();
    let before = round_trips();
    let mut after = solutions(&engine, &plan).await;
    let round_trips = round_trips() - before;
    client
        .batch_execute("DROP SCHEMA v12n_known CASCADE")
        .await
        .unwrap();
    let mut first = first;
    first.sort();
    after.sort();
    assert_eq!(after, first);
    // The check with the statement, reading the table again, then the
    // check with the new statement, and running it.
    assert_eq!(round_trips, 4);
}

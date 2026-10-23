// SPDX-License-Identifier: Apache-2.0
//! Runs against the `testenv` PostgreSQL. Skipped unless
//! `V12N_TEST_DATABASE_URL` is set, e.g.
//! `postgresql://v12n:v12n-dev@127.0.0.1:55432/shop`.

use std::time::Duration;

use tokio_postgres::NoTls;
use tokio_postgres::types::Type;
use v12n_core::mapping::{Mapping, MappingIndex};
use v12n_core::sql::SqlPlan;
use v12n_core::{sparql, unfold};
use v12n_postgres::{Database, Error, Limits};

const SHOP: &str = include_str!("../../../examples/shop/mapping.ttl");
const VIP_ORDERS: &str = include_str!("../../../examples/shop/queries/vip-orders.rq");

fn database_url() -> Option<String> {
    let url = std::env::var("V12N_TEST_DATABASE_URL").ok();
    if url.is_none() {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
    }
    url
}

fn plan(mapping: &str, query: &str) -> SqlPlan {
    let mapping = Mapping::from_turtle(mapping).unwrap();
    let query = sparql::parse(query).unwrap();
    unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap()
}

/// Each solution as `?var=term` pairs in N-Triples syntax, sorted, since
/// SPARQL results have no order without `ORDER BY`.
async fn run(url: &str, limits: Limits, plan: &SqlPlan) -> Result<(Vec<String>, bool), Error> {
    let database = Database::connect(url, limits).await?;
    let mut solutions = database.execute(plan).await?;
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
    rows.sort();
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
    // comparison stays on the integer column and can use its primary key.
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    let sql = plan.to_sql().sql;
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
        let database = Database::connect(&url, limits).await.unwrap();
        let mut solutions = database.execute(&plan).await.unwrap();
        assert!(solutions.try_next().await.unwrap().is_some());
        assert_eq!(solutions.is_truncated(), truncated, "row limit {row_limit}");
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

#[tokio::test]
async fn natural_mapping_covers_postgres_types() {
    let Some(url) = database_url() else { return };
    let columns = [
        "d", "f", "r", "b", "dt", "tm", "ts", "tz", "bin", "s", "u", "iv",
    ];
    let mapping = sql_query_mapping(
        r"SELECT 1 AS id, 12.50::numeric AS d, 1.5::float8 AS f, 0.1::real AS r,
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
        ("f", xsd("1.5E0", "double")),
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
        "d", "f", "b", "dt", "ts", "tz", "bin", "u", "iv", "i2", "i8",
    ];
    let mapping = sql_query_mapping(
        r"SELECT 1 AS id, 12.50::numeric AS d, 1.5::float8 AS f, true AS b,
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

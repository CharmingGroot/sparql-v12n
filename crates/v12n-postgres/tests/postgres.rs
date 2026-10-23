// SPDX-License-Identifier: Apache-2.0
//! Runs against the `testenv` PostgreSQL. Skipped unless
//! `V12N_TEST_DATABASE_URL` is set, e.g.
//! `postgresql://v12n:v12n-dev@127.0.0.1:55432/shop`.

use std::time::Duration;

use tokio_postgres::NoTls;
use tokio_postgres::types::Type;
use v12n_core::mapping::{Mapping, MappingIndex};
use v12n_core::solution::canonical_decimal;
use v12n_core::sql::{RenderOptions, SqlPlan};
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
    // comparison stays on the integer column and can use its primary key,
    // which also shows that no duplicates need removing.
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(connection);
    let database = Database::connect(&url, Limits::default()).await.unwrap();
    let keys = database.table_keys(&plan).await.unwrap();
    let sql = plan
        .to_sql_with(&RenderOptions {
            keys: keys.as_ref(),
            ..RenderOptions::default()
        })
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
/// subject `http://example.org/row/{id}`: subjects of different shapes
/// cannot share a variable across UNION branches yet.
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
    let database = Database::connect(&url, limits).await.unwrap();
    let mut solutions = database.execute(&plan(&mapping, query)).await.unwrap();
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
        matches!(&error, Error::Database(m) if m.contains("does not exist")),
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
    let database = Database::connect(&url, Limits::default()).await.unwrap();
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

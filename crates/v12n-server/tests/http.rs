// SPDX-License-Identifier: Apache-2.0
//! The `/sparql` endpoint through its router. Tests that reach the
//! database run against the `testenv` PostgreSQL and are skipped unless
//! `V12N_TEST_DATABASE_URL` is set, e.g.
//! `postgresql://v12n:v12n-dev@127.0.0.1:55432/shop`.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, Response, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;
use v12n_core::mapping::Mapping;
use v12n_core::{Limits, Planner};
use v12n_postgres::PostgresConnector;
use v12n_server::{App, ServerLimits, TRUNCATED_HEADER, router};

const SHOP: &str = include_str!("../../../examples/shop/mapping.ttl");
const VIP_ORDERS: &str = include_str!("../../../examples/shop/queries/vip-orders.rq");
const CUSTOMERS: &str = "SELECT ?c WHERE { ?c a <http://example.org/Customer> }";
const JSON: &str = "application/sparql-results+json";
/// Nothing listens on port 1, so a test that reaches the database fails
/// fast instead of hanging.
const NO_DATABASE: &str = "postgresql://v12n@127.0.0.1:1/shop?connect_timeout=2";

type PostgresApp = App<PostgresConnector>;

fn new_app(mapping: &str, url: &str, limits: ServerLimits) -> Arc<PostgresApp> {
    App::new(
        Planner::new(Mapping::from_turtle(mapping).unwrap()),
        PostgresConnector::new(url),
        limits,
    )
}

fn offline() -> Arc<PostgresApp> {
    new_app(SHOP, NO_DATABASE, ServerLimits::default())
}

fn encode(query: &str) -> String {
    form_urlencoded::byte_serialize(query.as_bytes()).collect()
}

fn get(query: &str) -> Request<Body> {
    Request::get(format!("/sparql?query={}", encode(query)))
        .header(header::ACCEPT, JSON)
        .body(Body::empty())
        .unwrap()
}

fn post_form(query: &str) -> Request<Body> {
    Request::post("/sparql")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::ACCEPT, JSON)
        .body(Body::from(format!("query={}", encode(query))))
        .unwrap()
}

fn post_direct(query: &str) -> Request<Body> {
    Request::post("/sparql")
        .header(header::CONTENT_TYPE, "application/sparql-query")
        .body(Body::from(query.to_owned()))
        .unwrap()
}

async fn send(app: &Arc<PostgresApp>, request: Request<Body>) -> Response<Body> {
    router(Arc::clone(app)).oneshot(request).await.unwrap()
}

async fn text(response: Response<Body>) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn status(app: &Arc<PostgresApp>, request: Request<Body>) -> StatusCode {
    send(app, request).await.status()
}

#[tokio::test]
async fn health_route_answers_without_a_database() {
    let response = send(
        &offline(),
        Request::get("/healthz").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn syntax_error_is_400_and_unsupported_feature_is_501() {
    let app = offline();
    let response = send(&app, get("SELECT ?x WHERE {")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(text(response).await.contains("query syntax error"));
    let response = send(&app, post_direct("ASK { ?s ?p ?o }")).await;
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert!(text(response).await.contains("ASK"));
}

fn get_as(query: &str, accept: &str) -> Request<Body> {
    Request::get(format!("/sparql?query={}", encode(query)))
        .header(header::ACCEPT, accept)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn unacceptable_format_is_406_before_the_query_is_read() {
    for accept in ["application/json", "text/html, */*;q=0", "text/csv;q=0"] {
        let response = send(&offline(), get_as("x", accept)).await;
        assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE, "{accept}");
        let body = text(response).await;
        assert!(
            body.contains("application/sparql-results+xml")
                && body.contains("text/tab-separated-values"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn every_offered_format_passes_negotiation() {
    // A syntax error is found after negotiation, so 400 shows that the
    // format was accepted.
    for accept in [
        "application/sparql-results+json",
        "application/sparql-results+xml",
        "text/csv",
        "text/tab-separated-values",
        "text/*;q=0.5, application/xml",
    ] {
        assert_eq!(
            status(&offline(), get_as("SELECT ?x WHERE {", accept)).await,
            StatusCode::BAD_REQUEST,
            "{accept}"
        );
    }
}

#[tokio::test]
async fn protocol_errors_map_to_their_statuses() {
    let app = offline();
    let missing = Request::get("/sparql").body(Body::empty()).unwrap();
    assert_eq!(status(&app, missing).await, StatusCode::BAD_REQUEST);
    let put = Request::put("/sparql").body(Body::empty()).unwrap();
    assert_eq!(status(&app, put).await, StatusCode::METHOD_NOT_ALLOWED);
    let plain = Request::post("/sparql")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(CUSTOMERS))
        .unwrap();
    assert_eq!(
        status(&app, plain).await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );

    let small = new_app(
        SHOP,
        NO_DATABASE,
        ServerLimits {
            max_query_bytes: 16,
            ..ServerLimits::default()
        },
    );
    assert_eq!(
        status(&small, get(CUSTOMERS)).await,
        StatusCode::URI_TOO_LONG
    );
    assert_eq!(
        status(&small, post_form(CUSTOMERS)).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        status(&small, post_direct(CUSTOMERS)).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn unreachable_database_is_500_without_the_connection_string() {
    let response = send(&offline(), get(CUSTOMERS)).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = text(response).await;
    assert!(body.starts_with("database error"), "{body}");
    assert!(!body.contains("postgresql://"), "{body}");
}

fn database_url() -> Option<String> {
    let url = std::env::var("V12N_TEST_DATABASE_URL").ok();
    if url.is_none() {
        eprintln!("skipped: V12N_TEST_DATABASE_URL is not set");
    }
    url
}

fn limits(row_limit: u64, statement_timeout: Duration) -> ServerLimits {
    ServerLimits {
        query: Limits {
            statement_timeout,
            row_limit,
        },
        ..ServerLimits::default()
    }
}

/// Solutions as sorted `?var=value` lines, since results have no order
/// without `ORDER BY`.
fn solutions(json: &Value) -> Vec<String> {
    let mut rows: Vec<String> = json["results"]["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            let mut pairs: Vec<String> = b
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("?{k}={}", v["value"].as_str().unwrap()))
                .collect();
            pairs.sort();
            pairs.join(" ")
        })
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn every_protocol_method_returns_the_same_streamed_results() {
    let Some(url) = database_url() else { return };
    let app = new_app(SHOP, &url, ServerLimits::default());
    let mut bodies = Vec::new();
    for request in [
        get(VIP_ORDERS),
        post_form(VIP_ORDERS),
        post_direct(VIP_ORDERS),
    ] {
        let response = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], JSON);
        assert!(response.headers().get(TRUNCATED_HEADER).is_none());
        let json: Value = serde_json::from_str(&text(response).await).unwrap();
        assert_eq!(json["head"]["vars"], serde_json::json!(["order", "amount"]));
        bodies.push(solutions(&json));
    }
    assert_eq!(
        bodies[0],
        [
            "?amount=120000 ?order=http://example.org/order/1001",
            "?amount=15000 ?order=http://example.org/order/1004",
            "?amount=87000 ?order=http://example.org/order/1003",
        ]
    );
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(bodies[0], bodies[2]);
}

#[tokio::test]
async fn row_limit_truncation_is_200_with_the_truncated_header() {
    let Some(url) = database_url() else { return };
    for (row_limit, header, rows) in [(2, Some("2"), 2), (3, None, 3), (0, None, 3)] {
        let app = new_app(SHOP, &url, limits(row_limit, Duration::from_secs(30)));
        let response = send(&app, get(CUSTOMERS)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(TRUNCATED_HEADER)
                .map(|v| v.to_str().unwrap()),
            header,
            "row limit {row_limit}"
        );
        let json: Value = serde_json::from_str(&text(response).await).unwrap();
        assert_eq!(solutions(&json).len(), rows, "row limit {row_limit}");
    }
    // A query LIMIT at or below the row limit cannot be truncated.
    for (limit, row_limit, header, rows) in [
        (2, 2, None, 2),
        (3, 2, Some("2"), 2),
        (3, 3, None, 3),
        (4, 3, None, 3),
    ] {
        let app = new_app(SHOP, &url, limits(row_limit, Duration::from_secs(30)));
        let query = format!("{CUSTOMERS} ORDER BY ?c LIMIT {limit}");
        let response = send(&app, get(&query)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(TRUNCATED_HEADER)
                .map(|v| v.to_str().unwrap()),
            header,
            "LIMIT {limit}, row limit {row_limit}"
        );
        let json: Value = serde_json::from_str(&text(response).await).unwrap();
        assert_eq!(
            solutions(&json).len(),
            rows,
            "LIMIT {limit}, row limit {row_limit}"
        );
    }
}

const XML: &str = "application/sparql-results+xml";
const CSV: &str = "text/csv";
const TSV: &str = "text/tab-separated-values";

/// The data lines of a CSV or TSV body, sorted, without the header.
fn sorted_lines(body: &str, eol: &str) -> Vec<String> {
    let mut lines: Vec<String> = body
        .strip_suffix(eol)
        .unwrap()
        .split(eol)
        .skip(1)
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

#[tokio::test]
async fn each_format_answers_with_its_content_type_and_syntax() {
    let Some(url) = database_url() else { return };
    let app = new_app(SHOP, &url, ServerLimits::default());
    let query = "PREFIX ex: <http://example.org/> \
                 SELECT ?order ?amount WHERE { ?order ex:amount ?amount } \
                 ORDER BY ?order LIMIT 2";

    let response = send(&app, get_as(query, XML)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], XML);
    assert_eq!(response.headers()[header::VARY], "accept");
    let body = text(response).await;
    assert!(
        body.starts_with(
            "<?xml version=\"1.0\"?>\n<sparql xmlns=\"http://www.w3.org/2005/sparql-results#\">\
             <head><variable name=\"order\"/><variable name=\"amount\"/></head><results>\n<result>\
             <binding name=\"order\"><uri>http://example.org/order/1001</uri></binding>\
             <binding name=\"amount\"><literal datatype=\"http://www.w3.org/2001/XMLSchema#integer\">"
        ) && body.ends_with("</result>\n</results></sparql>\n"),
        "{body}"
    );
    assert_eq!(body.matches("<result>").count(), 2, "{body}");

    let response = send(&app, get_as(query, "text/csv, */*;q=0.1")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/csv; charset=utf-8"
    );
    let body = text(response).await;
    assert!(body.starts_with("order,amount\r\n"), "{body}");
    assert_eq!(
        sorted_lines(&body, "\r\n"),
        [
            "http://example.org/order/1001,120000",
            "http://example.org/order/1002,30000"
        ]
    );

    let response = send(&app, get_as(query, TSV)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/tab-separated-values; charset=utf-8"
    );
    let body = text(response).await;
    assert!(body.starts_with("?order\t?amount\n"), "{body}");
    assert_eq!(
        sorted_lines(&body, "\n"),
        [
            "<http://example.org/order/1001>\t120000",
            "<http://example.org/order/1002>\t30000"
        ]
    );
}

#[tokio::test]
async fn truncation_header_is_the_same_in_every_format() {
    let Some(url) = database_url() else { return };
    let app = new_app(SHOP, &url, limits(2, Duration::from_secs(30)));
    for (accept, rows) in [
        (JSON, None),
        (XML, Some("<result>")),
        (CSV, Some("\r\n")),
        (TSV, Some("\n")),
    ] {
        let response = send(&app, get_as(CUSTOMERS, accept)).await;
        assert_eq!(response.status(), StatusCode::OK, "{accept}");
        assert_eq!(response.headers()[TRUNCATED_HEADER], "2", "{accept}");
        let body = text(response).await;
        let count = match rows {
            None => solutions(&serde_json::from_str(&body).unwrap()).len(),
            // CSV and TSV count the header line too.
            Some(eol) if eol.contains('\n') => body.matches(eol).count() - 1,
            Some(tag) => body.matches(tag).count(),
        };
        assert_eq!(count, 2, "{accept}: {body}");
    }
}

fn sleeping_mapping(seconds: f64) -> String {
    format!(
        "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
         <#Slow> rr:logicalTable [ rr:sqlQuery \"SELECT 1 AS id, 2 AS v FROM pg_sleep({seconds})\" ] ;\n\
         rr:subjectMap [ rr:template \"http://example.org/row/{{id}}\" ] ;\n\
         rr:predicateObjectMap [ rr:predicate <http://example.org/v> ; rr:objectMap [ rr:column \"v\" ] ] .\n"
    )
}

const SLOW_QUERY: &str = "SELECT ?v WHERE { ?s <http://example.org/v> ?v }";

#[tokio::test]
async fn timeout_before_the_first_row_is_503() {
    let Some(url) = database_url() else { return };
    let app = new_app(
        &sleeping_mapping(2.0),
        &url,
        limits(10, Duration::from_millis(200)),
    );
    let response = send(&app, get(SLOW_QUERY)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(text(response).await.contains("statement timeout"));
}

#[tokio::test]
async fn requests_over_the_concurrency_limit_are_refused_with_503() {
    let Some(url) = database_url() else { return };
    let app = new_app(
        &sleeping_mapping(1.0),
        &url,
        ServerLimits {
            max_concurrent_requests: 1,
            ..ServerLimits::default()
        },
    );
    let slow = tokio::spawn({
        let app = Arc::clone(&app);
        async move { send(&app, get(SLOW_QUERY)).await.status() }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let response = send(&app, get(SLOW_QUERY)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    assert_eq!(slow.await.unwrap(), StatusCode::OK);
    // Dropping the first response gave its permit back.
    assert_eq!(status(&app, get(SLOW_QUERY)).await, StatusCode::OK);
}

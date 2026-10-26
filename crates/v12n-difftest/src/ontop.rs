// SPDX-License-Identifier: Apache-2.0
//! Queries the Ontop SPARQL endpoint and reads its JSON results.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use oxrdf::{BlankNode, Literal, NamedNode, Term};
use serde_json::Value;

use crate::RETRY_LOG_VAR;
use crate::compare::{Results, Row};

/// Ontop runs reasoning and SQL generation per query; the first query
/// after startup also warms up the JVM.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Attempts after the first one for a response that [`is_transient`].
const MAX_RETRIES: u32 = 2;

/// Runs `query` on the endpoint (SPARQL 1.1 Protocol §2.1.2: POST with a
/// URL-encoded body). `label` names the query in retry reports.
pub fn query(endpoint: &str, label: &str, query: &str) -> Result<Results, String> {
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("query", query)
        .finish();
    let (status, response) = with_retries(
        || post(endpoint, &body),
        |attempt| report_retry(label, attempt),
    )?;
    if status != 200 {
        return Err(format!("HTTP {status}: {}", response.trim()));
    }
    parse_results(&response)
}

/// Ontop 5.5.0 sometimes fails a query with HTTP 500 and a
/// `ConcurrentModificationException` when it answers several queries at
/// once; the same query then succeeds. Any other failure is reported, so
/// that a real error is never retried away.
fn is_transient(status: u16, body: &str) -> bool {
    status == 500 && body.contains("ConcurrentModificationException")
}

/// Sends the request, and again at most [`MAX_RETRIES`] times while the
/// response [`is_transient`], calling `on_retry` with the number of the
/// next attempt (2 or 3) before each retry.
fn with_retries(
    mut send: impl FnMut() -> Result<(u16, String), String>,
    mut on_retry: impl FnMut(u32),
) -> Result<(u16, String), String> {
    let mut attempt = 1;
    loop {
        let (status, body) = send()?;
        if attempt > MAX_RETRIES || !is_transient(status, &body) {
            return Ok((status, body));
        }
        attempt += 1;
        on_retry(attempt);
    }
}

/// Test harnesses capture the output of passing tests, so the line also
/// goes to the file named by [`RETRY_LOG_VAR`], which `testenv/difftest.sh`
/// prints after each run.
fn report_retry(label: &str, attempt: u32) {
    let line = format!(
        "Ontop retry: {label}: HTTP 500 with ConcurrentModificationException, \
         attempt {attempt} of {}",
        MAX_RETRIES + 1
    );
    eprintln!("{line}");
    if let Some(path) = std::env::var_os(RETRY_LOG_VAR) {
        // One write per line, so that lines from tests running at the same
        // time do not interleave. A lost report must not fail the check.
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(format!("{line}\n").as_bytes()));
    }
}

/// A minimal HTTP/1.0 POST. An HTTP/1.0 response is never chunked and
/// ends when the server closes the connection, so no HTTP client library
/// is needed for a test that talks to one local endpoint.
fn post(endpoint: &str, body: &str) -> Result<(u16, String), String> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| format!("endpoint must be an http:// URL: {endpoint}"))?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
    let io = |e: std::io::Error| format!("{endpoint}: {e}");
    let mut stream = TcpStream::connect(authority).map_err(io)?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(io)?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(io)?;
    write!(
        stream,
        "POST {path} HTTP/1.0\r\n\
         Host: {authority}\r\n\
         Accept: application/sparql-results+json\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body}",
        body.len()
    )
    .map_err(io)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(io)?;
    let response = String::from_utf8(response).map_err(|e| format!("{endpoint}: {e}"))?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("{endpoint}: malformed HTTP response"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("{endpoint}: malformed status line"))?;
    Ok((status, body.to_owned()))
}

/// Reads SPARQL 1.1 Query Results JSON. Terms are rebuilt as RDF terms,
/// so that a literal written with `"datatype": xsd:string` and one
/// written without a datatype compare equal, as RDF 1.1 defines them.
pub fn parse_results(json: &str) -> Result<Results, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| format!("invalid JSON results: {e}"))?;
    let variables = value["head"]["vars"]
        .as_array()
        .ok_or("results have no head.vars")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or("non-string variable name")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let rows = value["results"]["bindings"]
        .as_array()
        .ok_or("results have no results.bindings")?
        .iter()
        .map(|binding| {
            binding
                .as_object()
                .ok_or_else(|| "a binding is not an object".to_owned())?
                .iter()
                .map(|(variable, term)| Ok((variable.clone(), parse_term(term)?)))
                .collect::<Result<Row, String>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Results { variables, rows })
}

fn parse_term(term: &Value) -> Result<Term, String> {
    let value = term["value"]
        .as_str()
        .ok_or_else(|| format!("term without a value: {term}"))?;
    let datatype = term.get("datatype").and_then(Value::as_str);
    let language = term.get("xml:lang").and_then(Value::as_str);
    match term["type"].as_str() {
        Some("uri") => NamedNode::new(value)
            .map(Into::into)
            .map_err(|e| e.to_string()),
        // "typed-literal" is from the 2008 W3C note that preceded SPARQL 1.1.
        Some("literal" | "typed-literal") => match (language, datatype) {
            (Some(language), _) => Literal::new_language_tagged_literal(value, language)
                .map(Into::into)
                .map_err(|e| e.to_string()),
            (None, Some(datatype)) => NamedNode::new(datatype)
                .map(|datatype| Literal::new_typed_literal(value, datatype).into())
                .map_err(|e| e.to_string()),
            (None, None) => Ok(Literal::new_simple_literal(value).into()),
        },
        Some("bnode") => BlankNode::new(value)
            .map(Into::into)
            .map_err(|e| e.to_string()),
        _ => Err(format!("unknown term type: {term}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_results_reads_every_term_kind_and_leaves_unbound_out() {
        let json = r#"{
          "head": { "vars": ["s", "o"] },
          "results": { "bindings": [
            { "s": { "type": "uri", "value": "http://example.org/a" },
              "o": { "type": "literal", "value": "4",
                     "datatype": "http://www.w3.org/2001/XMLSchema#integer" } },
            { "s": { "type": "uri", "value": "http://example.org/b" },
              "o": { "type": "literal", "value": "x", "xml:lang": "EN" } },
            { "s": { "type": "uri", "value": "http://example.org/c" } },
            { "o": { "type": "literal", "value": "y",
                     "datatype": "http://www.w3.org/2001/XMLSchema#string" } }
          ] }
        }"#;
        let results = parse_results(json).unwrap();
        assert_eq!(results.variables, ["s", "o"]);
        let rows: Vec<Vec<String>> = results
            .rows
            .iter()
            .map(|row| row.iter().map(|(k, v)| format!("?{k}={v}")).collect())
            .collect();
        assert_eq!(
            rows,
            [
                vec![
                    "?o=\"4\"^^<http://www.w3.org/2001/XMLSchema#integer>",
                    "?s=<http://example.org/a>"
                ],
                vec!["?o=\"x\"@en", "?s=<http://example.org/b>"],
                vec!["?s=<http://example.org/c>"],
                vec!["?o=\"y\""],
            ]
        );
    }

    const CME: &str = "java.util.ConcurrentModificationException\n\tat java.base/java.util.HashMap";

    #[test]
    fn only_http_500_with_a_concurrent_modification_is_transient() {
        assert!(is_transient(500, CME));
        assert!(!is_transient(
            500,
            "OntopReformulationException: unknown column"
        ));
        assert!(!is_transient(503, CME));
        assert!(!is_transient(200, CME));
        assert!(!is_transient(400, CME));
    }

    /// Sends the given responses in turn and records the retries.
    fn run(responses: &[(u16, &str)]) -> (Result<(u16, String), String>, usize, Vec<u32>) {
        let mut sent = 0;
        let mut retries = Vec::new();
        let result = with_retries(
            || {
                let (status, body) = responses[sent];
                sent += 1;
                Ok((status, body.to_owned()))
            },
            |attempt| retries.push(attempt),
        );
        (result, sent, retries)
    }

    #[test]
    fn a_transient_failure_is_retried_until_it_succeeds() {
        let (result, sent, retries) = run(&[(500, CME), (200, "{}")]);
        assert_eq!(result.unwrap(), (200, "{}".to_owned()));
        assert_eq!((sent, retries), (2, vec![2]));
    }

    #[test]
    fn a_transient_failure_is_retried_at_most_twice() {
        let (result, sent, retries) = run(&[(500, CME), (500, CME), (500, CME), (200, "{}")]);
        assert_eq!(result.unwrap().0, 500);
        assert_eq!((sent, retries), (3, vec![2, 3]));
    }

    #[test]
    fn other_failures_are_not_retried() {
        for response in [
            (500, "OntopReformulationException"),
            (503, CME),
            (400, "bad query"),
        ] {
            let (result, sent, retries) = run(&[response, (200, "{}")]);
            assert_eq!(result.unwrap().0, response.0);
            assert_eq!((sent, retries.len()), (1, 0), "{response:?}");
        }
    }

    #[test]
    fn connection_errors_are_not_retried() {
        let mut sent = 0;
        let result = with_retries(
            || {
                sent += 1;
                Err("connection refused".to_owned())
            },
            |_| panic!("no retry"),
        );
        assert_eq!(result.unwrap_err(), "connection refused");
        assert_eq!(sent, 1);
    }

    #[test]
    fn parse_results_rejects_unknown_term_types() {
        let json = r#"{ "head": { "vars": ["s"] }, "results": { "bindings": [
            { "s": { "type": "triple", "value": {} } } ] } }"#;
        assert!(parse_results(json).is_err());
    }
}

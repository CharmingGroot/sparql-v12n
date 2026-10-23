// SPDX-License-Identifier: Apache-2.0
//! Queries the Ontop SPARQL endpoint and reads its JSON results.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use oxrdf::{BlankNode, Literal, NamedNode, Term};
use serde_json::Value;

use crate::compare::{Results, Row};

/// Ontop runs reasoning and SQL generation per query; the first query
/// after startup also warms up the JVM.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Runs `query` on the endpoint (SPARQL 1.1 Protocol §2.1.2: POST with a
/// URL-encoded body).
pub fn query(endpoint: &str, query: &str) -> Result<Results, String> {
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("query", query)
        .finish();
    let (status, response) = post(endpoint, &body)?;
    if status != 200 {
        return Err(format!("HTTP {status}: {}", response.trim()));
    }
    parse_results(&response)
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

    #[test]
    fn parse_results_rejects_unknown_term_types() {
        let json = r#"{ "head": { "vars": ["s"] }, "results": { "bindings": [
            { "s": { "type": "triple", "value": {} } } ] } }"#;
        assert!(parse_results(json).is_err());
    }
}

// SPDX-License-Identifier: Apache-2.0
//! SPARQL 1.1 Protocol request handling: where the query comes from, and
//! which result format the client accepts.

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode, header};
use futures_util::StreamExt;
use v12n_core::results::ResultsFormat;

use crate::Failure;

const FORM: &str = "application/x-www-form-urlencoded";
const SPARQL_QUERY: &str = "application/sparql-query";
/// Percent-encoding at most triples the size of a query in a form body.
const FORM_OVERHEAD: usize = 3;

/// The results format for the `Accept` header, or `None` when it allows
/// none of [`ResultsFormat::ALL`]. Each format gets the quality of the most
/// specific media range that matches it (RFC 9110 §12.5.1), so `*/*` does
/// not override an explicit `q=0`. The highest quality wins, and a tie goes
/// to the earlier format, so `*/*` and a missing `Accept` give JSON.
pub(crate) fn negotiate(headers: &HeaderMap) -> Option<ResultsFormat> {
    let ranges: Vec<(String, f32)> = headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter(|range| !range.trim().is_empty())
        .map(|range| {
            let mut parts = range.split(';');
            let media = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
            let quality = parts
                .filter_map(|p| p.split_once('='))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
                .map(|(_, q)| q.trim().parse::<f32>().unwrap_or(1.0))
                .unwrap_or(1.0);
            (media, quality)
        })
        .collect();
    if ranges.is_empty() {
        return Some(ResultsFormat::Json);
    }
    let mut best: Option<(ResultsFormat, f32)> = None;
    for format in ResultsFormat::ALL {
        let quality = quality(&ranges, format.media_type());
        if quality > 0.0 && best.is_none_or(|(_, q)| quality > q) {
            best = Some((format, quality));
        }
    }
    best.map(|(format, _)| format)
}

/// The quality of the most specific range in `ranges` that matches
/// `media_type`; 0 when none does.
fn quality(ranges: &[(String, f32)], media_type: &str) -> f32 {
    let (kind, subtype) = media_type.split_once('/').unwrap_or_default();
    let mut best: Option<(u8, f32)> = None;
    for (media, quality) in ranges {
        let specificity = match media.split_once('/') {
            Some(("*", "*")) => 1,
            Some((k, "*")) if k == kind => 2,
            Some((k, s)) if k == kind && s == subtype => 3,
            _ => continue,
        };
        if best.is_none_or(|(s, _)| specificity > s) {
            best = Some((specificity, *quality));
        }
    }
    best.map_or(0.0, |(_, quality)| quality)
}

/// The query text of a request, by the three methods of SPARQL 1.1
/// Protocol §2.1: `GET` with a `query` parameter (§2.1.1), `POST` with a
/// URL-encoded form (§2.1.2), and `POST` with the query as the body
/// (§2.1.3).
pub(crate) async fn read_query(
    method: &Method,
    headers: &HeaderMap,
    url_query: Option<&str>,
    body: Body,
    max_query_bytes: usize,
) -> Result<String, Failure> {
    let url_params = parse_params(url_query.unwrap_or_default().as_bytes());
    reject_dataset(&url_params)?;
    if method == Method::GET {
        let query = single_query(url_params)?;
        if query.len() > max_query_bytes {
            return Err(Failure::new(
                StatusCode::URI_TOO_LONG,
                "query_too_large",
                format!("the query is longer than {max_query_bytes} bytes"),
            ));
        }
        return Ok(query);
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let mut parts = content_type.split(';');
    let essence = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    match essence.as_str() {
        FORM => {
            let limit = max_query_bytes.saturating_mul(FORM_OVERHEAD);
            let body = read_body(body, limit, max_query_bytes).await?;
            let params = parse_params(&body);
            reject_dataset(&params)?;
            let query = single_query(params)?;
            if query.len() > max_query_bytes {
                return Err(too_large(max_query_bytes));
            }
            Ok(query)
        }
        SPARQL_QUERY => {
            // §2.1.3: "UTF-8 is the only valid charset here."
            let charset = parts
                .filter_map(|p| p.split_once('='))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("charset"))
                .map(|(_, value)| value.trim().trim_matches('"'));
            if charset.is_some_and(|c| !c.eq_ignore_ascii_case("utf-8")) {
                return Err(Failure::new(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "content_type",
                    "application/sparql-query must be UTF-8",
                ));
            }
            if url_params.iter().any(|(name, _)| name == "query") {
                return Err(bad_request(
                    "the query is in the body; do not also pass a query parameter",
                ));
            }
            let body = read_body(body, max_query_bytes, max_query_bytes).await?;
            String::from_utf8(body).map_err(|_| bad_request("the query is not valid UTF-8"))
        }
        _ => Err(Failure::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type",
            format!("POST needs Content-Type {FORM} or {SPARQL_QUERY}"),
        )),
    }
}

fn parse_params(input: &[u8]) -> Vec<(String, String)> {
    form_urlencoded::parse(input).into_owned().collect()
}

/// §2.1.4 lets a service refuse a protocol dataset with 400. The engine
/// has one implicit dataset: the graph the mapping defines.
fn reject_dataset(params: &[(String, String)]) -> Result<(), Failure> {
    match params
        .iter()
        .find(|(name, _)| name == "default-graph-uri" || name == "named-graph-uri")
    {
        Some((name, _)) => Err(bad_request(format!(
            "{name} is not supported; the dataset is the one the mapping defines"
        ))),
        None => Ok(()),
    }
}

/// §2.1: a request carries exactly one `query`. Other parameters, such as
/// ones meant for other endpoints, are ignored.
fn single_query(params: Vec<(String, String)>) -> Result<String, Failure> {
    let mut queries = params.into_iter().filter(|(name, _)| name == "query");
    match (queries.next(), queries.next()) {
        (Some((_, query)), None) => Ok(query),
        (None, _) => Err(bad_request("missing query parameter")),
        (Some(_), Some(_)) => Err(bad_request("more than one query parameter")),
    }
}

/// Reads at most `limit` bytes, so an oversized body is refused without
/// being held in memory.
async fn read_body(body: Body, limit: usize, max_query_bytes: usize) -> Result<Vec<u8>, Failure> {
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| bad_request(format!("cannot read the body: {e}")))?;
        if out.len() + chunk.len() > limit {
            return Err(too_large(max_query_bytes));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn too_large(max_query_bytes: usize) -> Failure {
    Failure::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "query_too_large",
        format!("the query is longer than {max_query_bytes} bytes"),
    )
}

fn bad_request(message: impl Into<String>) -> Failure {
    Failure::new(StatusCode::BAD_REQUEST, "request", message)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn accept(values: &[&str]) -> Option<ResultsFormat> {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(header::ACCEPT, HeaderValue::from_str(value).unwrap());
        }
        negotiate(&headers)
    }

    #[test]
    fn accept_without_a_preference_gives_json() {
        let json = Some(ResultsFormat::Json);
        assert_eq!(accept(&[]), json);
        assert_eq!(accept(&[""]), json);
        assert_eq!(accept(&["*/*"]), json);
        assert_eq!(accept(&["application/*;q=0.5"]), json);
        assert_eq!(accept(&["text/html", "*/*;q=0.8"]), json);
        assert_eq!(accept(&["application/sparql-results+json"]), json);
        assert_eq!(
            accept(&["APPLICATION/SPARQL-RESULTS+JSON; charset=utf-8"]),
            json
        );
    }

    #[test]
    fn accept_picks_each_format_by_its_media_type() {
        assert_eq!(
            accept(&["application/sparql-results+xml"]),
            Some(ResultsFormat::Xml)
        );
        assert_eq!(accept(&["text/csv"]), Some(ResultsFormat::Csv));
        assert_eq!(
            accept(&["text/csv; charset=utf-8"]),
            Some(ResultsFormat::Csv)
        );
        assert_eq!(
            accept(&["Text/Tab-Separated-Values"]),
            Some(ResultsFormat::Tsv)
        );
        // Among the text types CSV comes first.
        assert_eq!(accept(&["text/*"]), Some(ResultsFormat::Csv));
        assert_eq!(accept(&["text/*, text/csv;q=0"]), Some(ResultsFormat::Tsv));
    }

    #[test]
    fn accept_follows_quality_values() {
        assert_eq!(
            accept(&["application/sparql-results+xml;q=1, application/sparql-results+json;q=0.9"]),
            Some(ResultsFormat::Xml)
        );
        assert_eq!(
            accept(&["text/csv;q=0.5, text/tab-separated-values;q=0.6"]),
            Some(ResultsFormat::Tsv)
        );
        // A tie goes to the server's order.
        assert_eq!(
            accept(&["text/tab-separated-values, application/sparql-results+xml"]),
            Some(ResultsFormat::Xml)
        );
        // The specific range overrides the wildcard, in both directions.
        assert_eq!(
            accept(&["application/sparql-results+json;q=0, */*"]),
            Some(ResultsFormat::Xml)
        );
        assert_eq!(accept(&["*/*;q=0.1", "text/csv"]), Some(ResultsFormat::Csv));
    }

    #[test]
    fn accept_refuses_other_formats_and_explicit_zero_quality() {
        assert_eq!(accept(&["application/json"]), None);
        assert_eq!(accept(&["text/html, application/xml"]), None);
        assert_eq!(accept(&["*/*;q=0"]), None);
        assert_eq!(accept(&["text/csv;q=0"]), None);
        assert_eq!(
            accept(&["application/*;q=0, text/csv;q=0, text/tab-separated-values;q=0"]),
            None
        );
    }

    async fn read(
        method: Method,
        content_type: Option<&str>,
        url_query: Option<&str>,
        body: &'static str,
    ) -> Result<String, (StatusCode, String)> {
        let mut headers = HeaderMap::new();
        if let Some(value) = content_type {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
        }
        read_query(&method, &headers, url_query, Body::from(body), 32)
            .await
            .map_err(|f| (f.status, f.message))
    }

    #[tokio::test]
    async fn read_query_takes_the_query_from_each_protocol_method() {
        let q = "SELECT * WHERE { ?s ?p ?o }";
        assert_eq!(
            read(
                Method::GET,
                None,
                Some("query=SELECT+*+WHERE+%7B+%3Fs+%3Fp+%3Fo+%7D&x=1"),
                ""
            )
            .await,
            Ok(q.to_owned())
        );
        assert_eq!(
            read(
                Method::POST,
                Some("application/x-www-form-urlencoded; charset=UTF-8"),
                None,
                "query=SELECT%20*%20WHERE%20%7B%20%3Fs%20%3Fp%20%3Fo%20%7D"
            )
            .await,
            Ok(q.to_owned())
        );
        assert_eq!(
            read(Method::POST, Some("application/sparql-query"), None, q).await,
            Ok(q.to_owned())
        );
    }

    #[tokio::test]
    async fn read_query_rejects_malformed_requests() {
        let status = |r: Result<String, (StatusCode, String)>| r.unwrap_err().0;
        let bad = StatusCode::BAD_REQUEST;
        assert_eq!(status(read(Method::GET, None, None, "").await), bad);
        assert_eq!(
            status(read(Method::GET, None, Some("query=a&query=b"), "").await),
            bad
        );
        assert_eq!(
            status(
                read(
                    Method::GET,
                    None,
                    Some("query=a&default-graph-uri=http%3A%2F%2Fg"),
                    ""
                )
                .await
            ),
            bad
        );
        assert_eq!(
            status(
                read(
                    Method::POST,
                    Some("application/x-www-form-urlencoded"),
                    None,
                    "update=x"
                )
                .await
            ),
            bad
        );
        assert_eq!(
            status(
                read(
                    Method::POST,
                    Some("application/sparql-query"),
                    Some("query=a"),
                    "b"
                )
                .await
            ),
            bad
        );
        assert_eq!(
            status(read(Method::POST, Some("text/plain"), None, "q").await),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            status(read(Method::POST, None, None, "q").await),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            status(
                read(
                    Method::POST,
                    Some("application/sparql-query; charset=latin1"),
                    None,
                    "q"
                )
                .await
            ),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[tokio::test]
    async fn read_query_enforces_the_size_limit() {
        let long = "SELECT * WHERE { ?s ?p ?o . ?o ?q ?r }";
        let status = |r: Result<String, (StatusCode, String)>| r.unwrap_err().0;
        assert_eq!(
            status(
                read(
                    Method::GET,
                    None,
                    Some(&format!("query={}", long.replace(' ', "+"))),
                    ""
                )
                .await
            ),
            StatusCode::URI_TOO_LONG
        );
        assert_eq!(
            status(read(Method::POST, Some("application/sparql-query"), None, long).await),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            status(
                read(
                    Method::POST,
                    Some("application/x-www-form-urlencoded"),
                    None,
                    "query=SELECT+*+WHERE+%7B+%3Fs+%3Fp+%3Fo+.+%3Fo+%3Fq+%3Fr+%7D"
                )
                .await
            ),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}

// SPDX-License-Identifier: Apache-2.0
//! Takes the TLS parameters out of a connection string, because
//! tokio-postgres rejects most of them.

use std::borrow::Cow;

use crate::Error;

/// The TLS parameters of a connection string, as written. A parameter
/// given twice keeps its last value, as in libpq.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct TlsParams {
    pub(super) mode: Option<String>,
    pub(super) root_cert: Option<String>,
    pub(super) cert: Option<String>,
    pub(super) key: Option<String>,
    pub(super) crl: Option<String>,
    pub(super) crl_dir: Option<String>,
}

impl TlsParams {
    /// Where the value of `key` goes, or `None` if tokio-postgres handles it.
    fn slot(&mut self, key: &str) -> Option<&mut Option<String>> {
        match key {
            "sslmode" => Some(&mut self.mode),
            "sslrootcert" => Some(&mut self.root_cert),
            "sslcert" => Some(&mut self.cert),
            "sslkey" => Some(&mut self.key),
            "sslcrl" => Some(&mut self.crl),
            "sslcrldir" => Some(&mut self.crl_dir),
            _ => None,
        }
    }
}

/// A connection string without its TLS parameters, and those parameters.
pub(super) type Split<'a> = (Cow<'a, str>, TlsParams);

/// Splits a libpq-style connection URL or `key=value` string.
pub(super) fn split(url: &str) -> Result<Split<'_>, Error> {
    match url
        .strip_prefix("postgresql://")
        .or_else(|| url.strip_prefix("postgres://"))
    {
        Some(after_scheme) => split_url(url, url.len() - after_scheme.len()),
        None => Ok(split_key_values(url)),
    }
}

/// Takes the TLS parameters out of the query of a URL. The query starts at
/// the first `?` after the first `@`, where tokio-postgres starts it too.
fn split_url(url: &str, after_scheme: usize) -> Result<Split<'_>, Error> {
    let after_credentials = url[after_scheme..]
        .find('@')
        .map_or(after_scheme, |at| after_scheme + at + 1);
    let Some(question) = url[after_credentials..].find('?') else {
        return Ok((Cow::Borrowed(url), TlsParams::default()));
    };
    let query_start = after_credentials + question + 1;
    let mut params = TlsParams::default();
    let mut kept = Vec::new();
    let mut query = &url[query_start..];
    while !query.is_empty() {
        // Like tokio-postgres: the key ends at `=`, the value at `&`.
        let Some(equals) = query.find('=') else {
            kept.push(query);
            break;
        };
        let end = query[equals..]
            .find('&')
            .map_or(query.len(), |i| equals + i);
        let (key, value) = (&query[..equals], &query[equals + 1..end]);
        let key = decode(key);
        let tls = key
            .as_deref()
            .and_then(|key| params.slot(key).map(|slot| (key, slot)));
        match tls {
            Some((key, slot)) => *slot = Some(decode_value(value, key)?),
            None => kept.push(&query[..end]),
        }
        query = query.get(end + 1..).unwrap_or("");
    }
    let mut rest = url[..query_start - 1].to_owned();
    if !kept.is_empty() {
        rest.push('?');
        rest.push_str(&kept.join("&"));
    }
    Ok((Cow::Owned(rest), params))
}

fn decode(value: &str) -> Option<String> {
    percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .ok()
        .map(Cow::into_owned)
}

fn decode_value(value: &str, key: &str) -> Result<String, Error> {
    decode(value).ok_or_else(|| Error::Config(format!("invalid {key}: not UTF-8")))
}

/// Takes the TLS parameters out of a `key=value` connection string and
/// writes the others back quoted. A string tokio-postgres cannot parse is
/// returned unchanged, so that its parser reports the error.
fn split_key_values(s: &str) -> Split<'_> {
    let Some(pairs) = key_values(s) else {
        return (Cow::Borrowed(s), TlsParams::default());
    };
    let mut params = TlsParams::default();
    let mut kept = Vec::new();
    for (key, value) in pairs {
        match params.slot(key) {
            Some(slot) => *slot = Some(value),
            None => {
                let quoted = value.replace('\\', "\\\\").replace('\'', "\\'");
                kept.push(format!("{key}='{quoted}'"));
            }
        }
    }
    (Cow::Owned(kept.join(" ")), params)
}

/// The parameters of a `key=value` connection string, read by the rules of
/// tokio-postgres's parser.
fn key_values(s: &str) -> Option<Vec<(&str, String)>> {
    let mut params = Vec::new();
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        let key_end = rest.find(|c: char| c.is_whitespace() || c == '=')?;
        let key = &rest[..key_end];
        if key.is_empty() {
            return None;
        }
        rest = rest[key_end..].trim_start().strip_prefix('=')?.trim_start();
        let (value, end) = value(rest)?;
        params.push((key, value));
        rest = rest[end..].trim_start();
    }
    Some(params)
}

/// The value at the start of `rest` and the byte offset after it: quoted
/// with `'`, or up to the next white space, with `\` escaping in both.
fn value(rest: &str) -> Option<(String, usize)> {
    let mut value = String::new();
    let mut chars = rest.char_indices();
    let quoted = rest.starts_with('\'');
    if quoted {
        chars.next();
    }
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => value.push(chars.next()?.1),
            '\'' if quoted => return Some((value, i + 1)),
            c if c.is_whitespace() && !quoted => return Some((value, i)),
            c => value.push(c),
        }
    }
    if quoted || value.is_empty() {
        return None;
    }
    Some((value, rest.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(mode: Option<&str>, root_cert: Option<&str>) -> TlsParams {
        TlsParams {
            mode: mode.map(str::to_owned),
            root_cert: root_cert.map(str::to_owned),
            ..TlsParams::default()
        }
    }

    fn split_owned(url: &str) -> (String, TlsParams) {
        let (rest, params) = split(url).unwrap();
        (rest.into_owned(), params)
    }

    #[test]
    fn url_parameters_are_taken_out() {
        assert_eq!(
            split_owned(
                "postgresql://u:p@h/db?sslmode=verify-full&application_name=x&sslrootcert=%2Ftmp%2Fca.pem"
            ),
            (
                "postgresql://u:p@h/db?application_name=x".to_owned(),
                params(Some("verify-full"), Some("/tmp/ca.pem"))
            )
        );
        assert_eq!(
            split_owned("postgresql://h/db?sslmode=require"),
            (
                "postgresql://h/db".to_owned(),
                params(Some("require"), None)
            )
        );
        assert_eq!(
            split_owned("postgresql://h/db"),
            ("postgresql://h/db".to_owned(), TlsParams::default())
        );
    }

    #[test]
    fn every_tls_parameter_is_taken_out_and_the_last_value_wins() {
        let (rest, params) = split_owned(
            "postgres://h/db?sslcert=%2Fc.pem&sslkey=/c.key&sslmode=prefer&sslcrl=/r.crl\
             &sslcrldir=/crls&connect_timeout=5&sslmode=allow",
        );
        assert_eq!(rest, "postgres://h/db?connect_timeout=5");
        assert_eq!(
            params,
            TlsParams {
                mode: Some("allow".to_owned()),
                root_cert: None,
                cert: Some("/c.pem".to_owned()),
                key: Some("/c.key".to_owned()),
                crl: Some("/r.crl".to_owned()),
                crl_dir: Some("/crls".to_owned()),
            }
        );
    }

    #[test]
    fn url_query_starts_after_the_credentials() {
        assert_eq!(
            split_owned("postgresql://u:p?ss@h/db?sslmode=disable"),
            (
                "postgresql://u:p?ss@h/db".to_owned(),
                params(Some("disable"), None)
            )
        );
    }

    #[test]
    fn key_value_parameters_are_taken_out_and_requoted() {
        assert_eq!(
            split_owned(
                "host=h password='a b\\'c' sslmode = verify-ca sslrootcert='/x y/ca.pem' dbname=shop"
            ),
            (
                "host='h' password='a b\\'c' dbname='shop'".to_owned(),
                params(Some("verify-ca"), Some("/x y/ca.pem"))
            )
        );
        let (_, all) = split_owned("sslcert=/c.pem sslkey='/k y.key' sslcrl=/r sslcrldir=/d");
        assert_eq!(
            (all.cert, all.key, all.crl, all.crl_dir),
            (
                Some("/c.pem".to_owned()),
                Some("/k y.key".to_owned()),
                Some("/r".to_owned()),
                Some("/d".to_owned())
            )
        );
    }

    #[test]
    fn malformed_key_values_are_left_unchanged() {
        for s in ["host=h password='unterminated", "host=", "=x"] {
            assert_eq!(split_owned(s), (s.to_owned(), TlsParams::default()));
        }
    }

    #[test]
    fn a_url_parameter_that_is_not_utf8_is_a_config_error() {
        let Err(error) = split("postgresql://h/db?sslcert=%FF") else {
            panic!("accepted a value that is not UTF-8");
        };
        assert!(error.to_string().contains("invalid sslcert"), "{error}");
    }
}

// SPDX-License-Identifier: Apache-2.0
//! TLS for the database connection: libpq's `sslmode` and `sslrootcert`,
//! which tokio-postgres only partly understands, on rustls.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{verify_server_cert_signed_by_trust_anchor, verify_server_name};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres::Config;
use tokio_postgres::config::SslMode as PostgresSslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::Error;

/// libpq's `sslmode`, see the PostgreSQL documentation, §32.19.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

/// Where the certificates that a server certificate must chain to come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Roots {
    /// The operating system's trust store.
    System,
    /// A PEM file, from `sslrootcert`.
    File(PathBuf),
}

/// The TLS settings of a connection string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tls {
    pub(crate) mode: SslMode,
    /// `sslrootcert`, if the connection string sets it.
    pub(crate) roots: Option<Roots>,
}

impl Tls {
    fn new(name: Option<&str>, root_cert: Option<&str>) -> Result<Self, Error> {
        let roots = root_cert.map(|value| match value {
            "system" => Roots::System,
            path => Roots::File(PathBuf::from(path)),
        });
        let mode = match name {
            None if roots == Some(Roots::System) => SslMode::VerifyFull,
            None | Some("prefer") => SslMode::Prefer,
            Some("disable") => SslMode::Disable,
            Some("require") => SslMode::Require,
            Some("verify-ca") => SslMode::VerifyCa,
            Some("verify-full") => SslMode::VerifyFull,
            Some("allow") => {
                return Err(Error::Config(
                    "sslmode \"allow\" is not supported (use \"prefer\" or \"disable\")".into(),
                ));
            }
            Some(other) => {
                return Err(Error::Config(format!("invalid sslmode \"{other}\"")));
            }
        };
        // libpq's rule: the system store holds every public CA, so a
        // certificate that chains to it proves nothing without the host name.
        if roots == Some(Roots::System) && !matches!(mode, SslMode::VerifyFull | SslMode::Disable) {
            let name = name.unwrap_or("prefer");
            return Err(Error::Config(format!(
                "weak sslmode \"{name}\" may not be used with sslrootcert=system (use \"verify-full\")"
            )));
        }
        Ok(Self { mode, roots })
    }

    /// The mode tokio-postgres negotiates with; the certificate checks of
    /// `verify-ca` and `verify-full` are the verifier's.
    fn postgres_mode(&self) -> PostgresSslMode {
        match self.mode {
            SslMode::Disable => PostgresSslMode::Disable,
            SslMode::Prefer => PostgresSslMode::Prefer,
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => PostgresSslMode::Require,
        }
    }

    /// The connector for these settings, or `None` without TLS.
    pub(crate) fn connector(&self) -> Result<Option<MakeRustlsConnect>, Error> {
        // libpq checks the chain in `prefer` and `require` too when a root
        // certificate file is given.
        let check = match (self.mode, &self.roots) {
            (SslMode::Disable, _) => return Ok(None),
            (SslMode::Prefer | SslMode::Require, None | Some(Roots::System)) => None,
            (SslMode::Prefer | SslMode::Require | SslMode::VerifyCa, _) => Some(false),
            (SslMode::VerifyFull, _) => Some(true),
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let roots = match check {
            None => None,
            Some(_) => Some(match &self.roots {
                Some(Roots::File(path)) => Arc::new(file_roots(path)?),
                None | Some(Roots::System) => system_roots()?,
            }),
        };
        let verifier = Verifier {
            roots,
            check_name: false,
            algorithms: provider.signature_verification_algorithms,
        };
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Database(format!("TLS setup failed: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        Ok(Some(MakeRustlsConnect::new(config)))
    }
}

/// Parses a libpq-style connection string or URL. tokio-postgres rejects
/// `sslrootcert` and the `verify-*` modes, so both are taken out of the
/// string before it parses the rest.
pub(crate) fn parse(url: &str) -> Result<(Config, Tls), Error> {
    let (rest, mode, root_cert) = match url
        .strip_prefix("postgresql://")
        .or_else(|| url.strip_prefix("postgres://"))
    {
        Some(after_scheme) => split_url(url, url.len() - after_scheme.len())?,
        None => split_key_values(url),
    };
    let tls = Tls::new(mode.as_deref(), root_cert.as_deref())?;
    let mut config: Config = rest.parse().map_err(Error::config)?;
    config.ssl_mode(tls.postgres_mode());
    // tokio-postgres refuses TLS without a host name, which only `host`
    // gives; with `hostaddr` alone the address stands in for it, as an
    // IP address name.
    if tls.mode != SslMode::Disable && config.get_hosts().is_empty() {
        let addresses = config.get_hostaddrs().to_vec();
        for address in addresses {
            config.host(address.to_string());
        }
    }
    Ok((config, tls))
}

type Split<'a> = (Cow<'a, str>, Option<String>, Option<String>);

/// Takes the TLS parameters out of the query of a URL. The query starts at
/// the first `?` after the first `@`, where tokio-postgres starts it too.
fn split_url(url: &str, after_scheme: usize) -> Result<Split<'_>, Error> {
    let after_credentials = url[after_scheme..]
        .find('@')
        .map_or(after_scheme, |at| after_scheme + at + 1);
    let Some(question) = url[after_credentials..].find('?') else {
        return Ok((Cow::Borrowed(url), None, None));
    };
    let query_start = after_credentials + question + 1;
    let (mut mode, mut root_cert) = (None, None);
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
        match decode(key).as_deref() {
            Some("sslmode") => mode = Some(decode_value(value, "sslmode")?),
            Some("sslrootcert") => root_cert = Some(decode_value(value, "sslrootcert")?),
            _ => kept.push(&query[..end]),
        }
        query = query.get(end + 1..).unwrap_or("");
    }
    let mut rest = url[..query_start - 1].to_owned();
    if !kept.is_empty() {
        rest.push('?');
        rest.push_str(&kept.join("&"));
    }
    Ok((Cow::Owned(rest), mode, root_cert))
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
    let Some(params) = key_values(s) else {
        return (Cow::Borrowed(s), None, None);
    };
    let (mut mode, mut root_cert) = (None, None);
    let mut kept = Vec::new();
    for (key, value) in params {
        match key {
            "sslmode" => mode = Some(value),
            "sslrootcert" => root_cert = Some(value),
            _ => {
                let quoted = value.replace('\\', "\\\\").replace('\'', "\\'");
                kept.push(format!("{key}='{quoted}'"));
            }
        }
    }
    (Cow::Owned(kept.join(" ")), mode, root_cert)
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
        let mut value = String::new();
        let mut chars = rest.char_indices();
        let quoted = rest.starts_with('\'');
        if quoted {
            chars.next();
        }
        let mut end = None;
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => value.push(chars.next()?.1),
                '\'' if quoted => {
                    end = Some(i + 1);
                    break;
                }
                c if c.is_whitespace() && !quoted => {
                    end = Some(i);
                    break;
                }
                c => value.push(c),
            }
        }
        let end = match end {
            Some(end) => end,
            None if quoted => return None,
            None => rest.len(),
        };
        if !quoted && value.is_empty() {
            return None;
        }
        params.push((key, value));
        rest = rest[end..].trim_start();
    }
    Some(params)
}

fn file_roots(path: &std::path::Path) -> Result<RootCertStore, Error> {
    let shown = path.display();
    let read_error = |e: &dyn std::fmt::Display| {
        Error::Database(format!(
            "cannot read the root certificate file {shown}: {e}"
        ))
    };
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(path).map_err(|e| read_error(&e))? {
        let cert = cert.map_err(|e| read_error(&e))?;
        roots.add(cert).map_err(|e| read_error(&e))?;
    }
    if roots.is_empty() {
        return Err(Error::Database(format!(
            "the root certificate file {shown} holds no certificate"
        )));
    }
    Ok(roots)
}

/// Read once per process: loading the store can take a while, and a
/// server opens a connection per pooled client.
fn system_roots() -> Result<Arc<RootCertStore>, Error> {
    static ROOTS: OnceLock<Result<Arc<RootCertStore>, String>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let loaded = rustls_native_certs::load_native_certs();
            let mut roots = RootCertStore::empty();
            roots.add_parsable_certificates(loaded.certs);
            if roots.is_empty() {
                let mut message = "no root certificates found in the system store".to_owned();
                if let Some(error) = loaded.errors.first() {
                    message.push_str(&format!(": {error}"));
                }
                return Err(message);
            }
            Ok(Arc::new(roots))
        })
        .clone()
        .map_err(Error::Database)
}

/// Checks the server certificate as `sslmode` asks. Without roots it
/// accepts any certificate, as libpq does in `prefer` and `require`; the
/// handshake signatures are checked in every mode.
#[derive(Debug)]
struct Verifier {
    roots: Option<Arc<RootCertStore>>,
    /// `verify-full`: the certificate must name the host.
    check_name: bool,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if let Some(roots) = &self.roots {
            let cert = ParsedCertificate::try_from(end_entity)?;
            verify_server_cert_signed_by_trust_anchor(
                &cert,
                roots,
                intermediates,
                now,
                self.algorithms.all,
            )?;
            if self.check_name {
                verify_server_name(&cert, server_name)?;
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(url: &str) -> (String, Option<String>, Option<String>) {
        let (rest, mode, root) = match url.strip_prefix("postgresql://") {
            Some(after) => split_url(url, url.len() - after.len()).unwrap(),
            None => split_key_values(url),
        };
        (rest.into_owned(), mode, root)
    }

    #[test]
    fn url_parameters_are_taken_out() {
        assert_eq!(
            split(
                "postgresql://u:p@h/db?sslmode=verify-full&application_name=x&sslrootcert=%2Ftmp%2Fca.pem"
            ),
            (
                "postgresql://u:p@h/db?application_name=x".to_owned(),
                Some("verify-full".to_owned()),
                Some("/tmp/ca.pem".to_owned())
            )
        );
        assert_eq!(
            split("postgresql://h/db?sslmode=require"),
            (
                "postgresql://h/db".to_owned(),
                Some("require".to_owned()),
                None
            )
        );
        assert_eq!(
            split("postgresql://h/db"),
            ("postgresql://h/db".to_owned(), None, None)
        );
    }

    #[test]
    fn url_query_starts_after_the_credentials() {
        assert_eq!(
            split("postgresql://u:p?ss@h/db?sslmode=disable"),
            (
                "postgresql://u:p?ss@h/db".to_owned(),
                Some("disable".to_owned()),
                None
            )
        );
    }

    #[test]
    fn key_value_parameters_are_taken_out_and_requoted() {
        assert_eq!(
            split(
                "host=h password='a b\\'c' sslmode = verify-ca sslrootcert='/x y/ca.pem' dbname=shop"
            ),
            (
                "host='h' password='a b\\'c' dbname='shop'".to_owned(),
                Some("verify-ca".to_owned()),
                Some("/x y/ca.pem".to_owned())
            )
        );
        let (config, tls) = parse("host=h password='a b\\'c' sslmode=verify-ca").unwrap();
        assert_eq!(config.get_password(), Some(&b"a b'c"[..]));
        assert_eq!(tls.mode, SslMode::VerifyCa);
        assert_eq!(config.get_ssl_mode(), PostgresSslMode::Require);
    }

    #[test]
    fn malformed_key_values_are_left_to_tokio_postgres() {
        let s = "host=h password='unterminated";
        assert_eq!(split(s), (s.to_owned(), None, None));
        assert!(matches!(parse(s), Err(Error::Config(_))));
    }

    #[test]
    fn modes_follow_libpq() {
        let mode = |url: &str| parse(url).map(|(_, tls)| tls.mode);
        assert_eq!(mode("postgresql://h/db").unwrap(), SslMode::Prefer);
        assert_eq!(
            mode("postgresql://h/db?sslrootcert=system").unwrap(),
            SslMode::VerifyFull
        );
        assert_eq!(
            mode("postgresql://h/db?sslmode=disable&sslrootcert=system").unwrap(),
            SslMode::Disable
        );
        for weak in ["prefer", "require", "verify-ca"] {
            let error = mode(&format!(
                "postgresql://h/db?sslmode={weak}&sslrootcert=system"
            ))
            .unwrap_err();
            assert!(error.to_string().contains("weak sslmode"), "{error}");
        }
        for invalid in ["allow", "verify_full"] {
            let error = mode(&format!("postgresql://h/db?sslmode={invalid}")).unwrap_err();
            assert!(matches!(error, Error::Config(_)), "{error}");
        }
    }

    #[test]
    fn a_missing_root_certificate_file_is_reported_with_its_path() {
        let (_, tls) =
            parse("postgresql://h/db?sslmode=verify-full&sslrootcert=/nonexistent/ca.pem").unwrap();
        let Err(error) = tls.connector() else {
            panic!("connector built without the root certificate file");
        };
        assert!(matches!(error, Error::Database(_)), "{error}");
        assert!(error.to_string().contains("/nonexistent/ca.pem"), "{error}");
    }
}

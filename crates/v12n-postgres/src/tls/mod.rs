// SPDX-License-Identifier: Apache-2.0
//! TLS for the database connection: libpq's `sslmode`, `sslrootcert`,
//! `sslcert`, `sslkey`, `sslcrl` and `sslcrldir`, which tokio-postgres
//! only partly understands, on rustls.

mod files;
mod params;
mod verifier;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::WantsClientCert;
use rustls::{ClientConfig, ConfigBuilder};
use tokio_postgres::config::SslMode as PostgresSslMode;
use tokio_postgres::{Client, Config, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;
use webpki::CertRevocationList;

use crate::Error;
use params::TlsParams;
use verifier::{Trust, Verifier};

/// libpq's `sslmode`, see the PostgreSQL documentation, §32.19.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SslMode {
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    const ALL: [Self; 6] = [
        Self::Disable,
        Self::Allow,
        Self::Prefer,
        Self::Require,
        Self::VerifyCa,
        Self::VerifyFull,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Allow => "allow",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    /// libpq's default is `prefer`, or `verify-full` with
    /// `sslrootcert=system`.
    fn from_param(name: Option<&str>, roots: Option<&Roots>) -> Result<Self, Error> {
        match name {
            None if roots == Some(&Roots::System) => Ok(Self::VerifyFull),
            None => Ok(Self::Prefer),
            Some(name) => Self::ALL
                .into_iter()
                .find(|mode| mode.name() == name)
                .ok_or_else(|| Error::Config(format!("invalid sslmode \"{name}\""))),
        }
    }
}

/// Where the certificates that a server certificate must chain to come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Roots {
    /// The operating system's trust store.
    System,
    /// A PEM file, from `sslrootcert`.
    File(PathBuf),
}

impl Roots {
    fn from_param(value: String) -> Self {
        match value.as_str() {
            "system" => Self::System,
            _ => Self::File(PathBuf::from(value)),
        }
    }

    fn load(&self) -> Result<Arc<rustls::RootCertStore>, Error> {
        match self {
            Self::File(path) => Ok(Arc::new(files::file_roots(path)?)),
            Self::System => files::system_roots(),
        }
    }
}

/// The certificate and key a client authenticates with, from `sslcert`
/// and `sslkey`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientCert {
    cert: PathBuf,
    key: PathBuf,
}

impl ClientCert {
    /// libpq falls back to `~/.postgresql/postgresql.crt` and `.key`; v12n
    /// reads no file the connection string does not name, so each needs
    /// the other.
    fn from_params(cert: Option<String>, key: Option<String>) -> Result<Option<Self>, Error> {
        match (cert, key) {
            (None, None) => Ok(None),
            (Some(cert), Some(key)) => Ok(Some(Self {
                cert: cert.into(),
                key: key.into(),
            })),
            (Some(_), None) => Err(Error::Config("sslcert requires sslkey".into())),
            (None, Some(_)) => Err(Error::Config("sslkey requires sslcert".into())),
        }
    }
}

/// Where certificate revocation lists come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CrlSource {
    /// `sslcrl`: a PEM file.
    File(PathBuf),
    /// `sslcrldir`: an OpenSSL hashed directory.
    Dir(PathBuf),
}

impl CrlSource {
    fn from_params(file: Option<String>, dir: Option<String>) -> Vec<Self> {
        let file = file.map(|path| Self::File(path.into()));
        let dir = dir.map(|path| Self::Dir(path.into()));
        file.into_iter().chain(dir).collect()
    }

    fn load(&self) -> Result<Vec<CertRevocationList<'static>>, Error> {
        match self {
            Self::File(path) => files::crl_file(path),
            Self::Dir(path) => files::crl_dir(path),
        }
    }
}

/// How much of the server certificate is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Nothing,
    Chain,
    ChainAndName,
}

/// The TLS settings of a connection string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tls {
    mode: SslMode,
    /// `sslrootcert`, if the connection string sets it.
    roots: Option<Roots>,
    client_cert: Option<ClientCert>,
    crls: Vec<CrlSource>,
}

impl Tls {
    fn new(params: TlsParams) -> Result<Self, Error> {
        let roots = params.root_cert.map(Roots::from_param);
        let tls = Self {
            mode: SslMode::from_param(params.mode.as_deref(), roots.as_ref())?,
            roots,
            client_cert: ClientCert::from_params(params.cert, params.key)?,
            crls: CrlSource::from_params(params.crl, params.crl_dir),
        };
        tls.check_system_roots()?;
        tls.check_crls_are_used()?;
        Ok(tls)
    }

    /// libpq's rule: the system store holds every public CA, so a
    /// certificate that chains to it proves nothing without the host name.
    fn check_system_roots(&self) -> Result<(), Error> {
        if self.roots == Some(Roots::System)
            && !matches!(self.mode, SslMode::VerifyFull | SslMode::Disable)
        {
            return Err(Error::Config(format!(
                "weak sslmode \"{}\" may not be used with sslrootcert=system (use \"verify-full\")",
                self.mode.name()
            )));
        }
        Ok(())
    }

    /// libpq ignores the CRLs when it checks no chain; a revocation list
    /// that is silently not applied would look like protection, so this is
    /// an error instead.
    fn check_crls_are_used(&self) -> Result<(), Error> {
        if !self.crls.is_empty() && self.mode != SslMode::Disable && self.check() == Check::Nothing
        {
            return Err(Error::Config(
                "sslcrl and sslcrldir need a certificate check: set sslrootcert to a file, \
                 or sslmode to \"verify-ca\" or \"verify-full\""
                    .into(),
            ));
        }
        Ok(())
    }

    fn check(&self) -> Check {
        // libpq checks the chain in the modes below `verify-ca` too when a
        // root certificate file is given.
        match (self.mode, &self.roots) {
            (SslMode::Disable, _) => Check::Nothing,
            (SslMode::Allow | SslMode::Prefer | SslMode::Require, None | Some(Roots::System)) => {
                Check::Nothing
            }
            (SslMode::Allow | SslMode::Prefer | SslMode::Require | SslMode::VerifyCa, _) => {
                Check::Chain
            }
            (SslMode::VerifyFull, _) => Check::ChainAndName,
        }
    }

    /// The mode tokio-postgres negotiates with. `allow` starts without TLS
    /// (see [`Tls::connect`]); the certificate checks of `verify-ca` and
    /// `verify-full` are the verifier's.
    fn postgres_mode(&self) -> PostgresSslMode {
        match self.mode {
            SslMode::Disable | SslMode::Allow => PostgresSslMode::Disable,
            SslMode::Prefer => PostgresSslMode::Prefer,
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => PostgresSslMode::Require,
        }
    }

    fn trust(&self) -> Result<Option<Trust>, Error> {
        let check_name = match self.check() {
            Check::Nothing => return Ok(None),
            Check::Chain => false,
            Check::ChainAndName => true,
        };
        let roots = self.roots.as_ref().unwrap_or(&Roots::System).load()?;
        let mut crls = Vec::new();
        for source in &self.crls {
            crls.extend(source.load()?);
        }
        Ok(Some(Trust {
            roots,
            crls,
            check_name,
        }))
    }

    /// The connector for these settings, or `None` without TLS.
    fn connector(&self) -> Result<Option<MakeRustlsConnect>, Error> {
        if self.mode == SslMode::Disable {
            return Ok(None);
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Verifier {
            trust: self.trust()?,
            algorithms: provider.signature_verification_algorithms,
        };
        let builder = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Database(format!("TLS setup failed: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier));
        Ok(Some(MakeRustlsConnect::new(self.client_auth(builder)?)))
    }

    fn client_auth(
        &self,
        builder: ConfigBuilder<ClientConfig, WantsClientCert>,
    ) -> Result<ClientConfig, Error> {
        let Some(client) = &self.client_cert else {
            return Ok(builder.with_no_client_auth());
        };
        let (chain, key) = files::client_identity(&client.cert, &client.key)?;
        builder.with_client_auth_cert(chain, key).map_err(|e| {
            Error::Database(format!(
                "cannot use the client certificate {} with the key {}: {e}",
                client.cert.display(),
                client.key.display()
            ))
        })
    }

    /// Opens a session with these settings; the connection task runs on
    /// the current Tokio runtime.
    pub(crate) async fn connect(
        &self,
        config: &Config,
        timeout: Duration,
    ) -> Result<Client, Error> {
        let failed = |e| Error::from_postgres(e, timeout);
        let Some(connector) = self.connector()? else {
            return plain(config).await.map_err(failed);
        };
        if self.mode != SslMode::Allow {
            return encrypted(config, connector).await.map_err(failed);
        }
        match plain(config).await {
            // libpq's `allow` tries TLS when the server answers the plain
            // startup with an error, such as a pg_hba.conf that admits the
            // user over `hostssl` only, and not after a network error
            // (fe-connect.c).
            Err(refused) if refused.as_db_error().is_some() => {
                let mut config = config.clone();
                config.ssl_mode(PostgresSslMode::Require);
                encrypted(&config, connector).await.map_err(failed)
            }
            other => other.map_err(failed),
        }
    }
}

async fn plain(config: &Config) -> Result<Client, tokio_postgres::Error> {
    let (client, connection) = config.connect(NoTls).await?;
    // Connection failures also surface as errors on the client's next
    // request, so the task's own result adds nothing.
    tokio::spawn(connection);
    Ok(client)
}

async fn encrypted(
    config: &Config,
    connector: MakeRustlsConnect,
) -> Result<Client, tokio_postgres::Error> {
    let (client, connection) = config.connect(connector).await?;
    tokio::spawn(connection);
    Ok(client)
}

/// Parses a libpq-style connection string or URL. tokio-postgres rejects
/// the `verify-*` modes and the file parameters, so they are taken out of
/// the string before it parses the rest.
pub(crate) fn parse(url: &str) -> Result<(Config, Tls), Error> {
    let (rest, params) = params::split(url)?;
    let tls = Tls::new(params)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_value_strings_keep_their_other_parameters() {
        let (config, tls) = parse("host=h password='a b\\'c' sslmode=verify-ca").unwrap();
        assert_eq!(config.get_password(), Some(&b"a b'c"[..]));
        assert_eq!(tls.mode, SslMode::VerifyCa);
        assert_eq!(config.get_ssl_mode(), PostgresSslMode::Require);
    }

    #[test]
    fn malformed_key_values_are_reported_by_tokio_postgres() {
        assert!(matches!(
            parse("host=h password='unterminated"),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn modes_follow_libpq() {
        let mode = |url: &str| parse(url).map(|(_, tls)| tls.mode);
        assert_eq!(mode("postgresql://h/db").unwrap(), SslMode::Prefer);
        assert_eq!(
            mode("postgresql://h/db?sslmode=allow").unwrap(),
            SslMode::Allow
        );
        assert_eq!(
            mode("postgresql://h/db?sslrootcert=system").unwrap(),
            SslMode::VerifyFull
        );
        assert_eq!(
            mode("postgresql://h/db?sslmode=disable&sslrootcert=system").unwrap(),
            SslMode::Disable
        );
        for weak in ["allow", "prefer", "require", "verify-ca"] {
            let error = mode(&format!(
                "postgresql://h/db?sslmode={weak}&sslrootcert=system"
            ))
            .unwrap_err();
            assert!(error.to_string().contains("weak sslmode"), "{error}");
        }
        for invalid in ["verify_full", "Require", ""] {
            let error = mode(&format!("postgresql://h/db?sslmode={invalid}")).unwrap_err();
            assert!(matches!(error, Error::Config(_)), "{error}");
        }
    }

    #[test]
    fn allow_starts_without_tls_and_checks_like_prefer() {
        let (config, tls) = parse("postgresql://h/db?sslmode=allow").unwrap();
        assert_eq!(config.get_ssl_mode(), PostgresSslMode::Disable);
        assert_eq!(tls.check(), Check::Nothing);
        let (_, tls) = parse("postgresql://h/db?sslmode=allow&sslrootcert=/ca.pem").unwrap();
        assert_eq!(tls.check(), Check::Chain);
        // The TLS attempt needs a server name, as in `prefer`.
        let (config, _) = parse("postgresql://u@/db?hostaddr=127.0.0.1&sslmode=allow").unwrap();
        assert!(!config.get_hosts().is_empty());
    }

    #[test]
    fn a_client_certificate_needs_both_files() {
        for (params, missing) in [("sslcert=/c.pem", "sslkey"), ("sslkey=/c.key", "sslcert")] {
            let Err(error) = parse(&format!("postgresql://h/db?{params}")) else {
                panic!("accepted {params} alone");
            };
            assert!(matches!(error, Error::Config(_)), "{error}");
            assert!(error.to_string().contains(missing), "{error}");
        }
        let (_, tls) = parse("postgresql://h/db?sslcert=/c.pem&sslkey=/c.key").unwrap();
        assert_eq!(
            tls.client_cert,
            Some(ClientCert {
                cert: "/c.pem".into(),
                key: "/c.key".into()
            })
        );
    }

    #[test]
    fn revocation_lists_need_a_certificate_check() {
        for unchecked in [
            "sslcrl=/r.crl",
            "sslcrldir=/crls&sslmode=require",
            "sslcrl=/r.crl&sslmode=allow",
        ] {
            let Err(error) = parse(&format!("postgresql://h/db?{unchecked}")) else {
                panic!("accepted {unchecked}");
            };
            assert!(matches!(error, Error::Config(_)), "{error}");
        }
        for checked in [
            "sslcrl=/r.crl&sslmode=verify-ca",
            "sslcrl=/r.crl&sslrootcert=/ca.pem",
            "sslcrldir=/crls&sslrootcert=system",
            "sslcrl=/r.crl&sslmode=disable",
        ] {
            parse(&format!("postgresql://h/db?{checked}")).unwrap();
        }
        let (_, tls) =
            parse("postgresql://h/db?sslmode=verify-full&sslcrl=/r&sslcrldir=/d").unwrap();
        assert_eq!(
            tls.crls,
            [CrlSource::File("/r".into()), CrlSource::Dir("/d".into())]
        );
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

    #[test]
    fn a_missing_client_certificate_is_reported_with_its_path() {
        let (_, tls) =
            parse("postgresql://h/db?sslmode=require&sslcert=/nonexistent/c.pem&sslkey=/c.key")
                .unwrap();
        let Err(error) = tls.connector() else {
            panic!("connector built without the client certificate file");
        };
        assert!(matches!(error, Error::Database(_)), "{error}");
        assert!(error.to_string().contains("/nonexistent/c.pem"), "{error}");
    }
}

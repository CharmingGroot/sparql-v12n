// SPDX-License-Identifier: Apache-2.0
//! TLS against a PostgreSQL with `ssl=on` and a server certificate for
//! `localhost` from a test CA, as `testenv/tls.sh up` starts it. Skipped
//! unless `V12N_TEST_TLS_DATABASE_URL` (a URL with the host `localhost` and
//! a query string) and `V12N_TEST_TLS_CERTS` (the directory with the
//! certificates and CRLs `tls.sh` generates) are set.

use std::sync::Arc;

use v12n_core::Limits;
use v12n_core::backend::{Backend, SolutionStream};
use v12n_core::catalog::{CatalogPlan, CatalogRequest};
use v12n_core::mapping::{Mapping, MappingIndex};
use v12n_core::sql::SqlPlan;
use v12n_core::{sparql, unfold};
use v12n_postgres::{Error, PostgresBackend};

/// Whether the session itself runs over TLS, by the server's account.
const SSL_IN_USE: &str = "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
    <#M> rr:logicalTable [ rr:sqlQuery \"\"\"SELECT pid, ssl FROM pg_stat_ssl \
      WHERE pid = pg_backend_pid()\"\"\" ] ;\n\
      rr:subjectMap [ rr:template \"http://example.org/connection/{pid}\" ] ;\n\
      rr:predicateObjectMap [ rr:predicate <http://example.org/ssl> ; \
      rr:objectMap [ rr:column \"ssl\" ] ] .";

struct Server {
    url: String,
    certs: String,
}

fn server() -> Option<Server> {
    let (Ok(url), Ok(certs)) = (
        std::env::var("V12N_TEST_TLS_DATABASE_URL"),
        std::env::var("V12N_TEST_TLS_CERTS"),
    ) else {
        eprintln!("skipped: V12N_TEST_TLS_DATABASE_URL or V12N_TEST_TLS_CERTS is not set");
        return None;
    };
    Some(Server { url, certs })
}

impl Server {
    fn with(&self, params: &str) -> String {
        format!("{}&{params}", self.url)
    }

    fn root_cert(&self, file: &str) -> String {
        format!("sslrootcert={}/{file}", self.certs)
    }

    /// `verify-full` against the test CA, plus `params`.
    fn verified(&self, params: &str) -> String {
        self.with(&format!(
            "sslmode=verify-full&{}&{params}",
            self.root_cert("ca.pem")
        ))
    }

    /// The URL for the role that `tls.sh` admits over TLS with a client
    /// certificate only.
    fn cert_role(&self, params: &str) -> String {
        let (scheme, rest) = self.url.split_once("://").unwrap();
        let (_, address) = rest.split_once('@').expect("the URL must hold credentials");
        format!("{scheme}://v12n_cert@{address}&{params}")
    }

    fn client_cert(&self) -> String {
        format!("sslcert={0}/client.pem&sslkey={0}/client.key", self.certs)
    }
}

fn plan() -> SqlPlan {
    let mapping = Mapping::from_turtle(SSL_IN_USE).unwrap();
    let query = sparql::parse("SELECT ?ssl WHERE { ?c <http://example.org/ssl> ?ssl }").unwrap();
    unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap()
}

/// Connects and reports whether the session uses TLS.
async fn ssl_in_use(url: &str) -> Result<bool, Error> {
    let database = PostgresBackend::connect(url, Limits::default()).await?;
    let plan = plan();
    let snapshot = database
        .load_catalog(&CatalogRequest::for_plan(&plan))
        .await?;
    let mut solutions = database
        .execute(&CatalogPlan::new(&plan, Arc::new(snapshot))?)
        .await?;
    let solution = solutions.try_next().await?.expect("one row");
    let [Some(ssl)] = solution.values() else {
        panic!("ssl is unbound");
    };
    Ok(ssl.to_string().starts_with("\"true\""))
}

fn assert_rejected(result: Result<bool, Error>, reason: &str) {
    match result {
        Err(Error::Database(message)) => {
            assert!(message.contains(reason), "{message}");
            assert!(!message.contains("v12n-dev"), "{message}");
        }
        other => panic!("expected a database error with {reason:?}, got {other:?}"),
    }
}

#[tokio::test]
async fn require_encrypts_without_checking_the_certificate() {
    let Some(server) = server() else { return };
    assert!(ssl_in_use(&server.with("sslmode=require")).await.unwrap());
}

#[tokio::test]
async fn prefer_is_the_default_and_uses_tls_when_offered() {
    let Some(server) = server() else { return };
    assert!(ssl_in_use(&server.url).await.unwrap());
}

#[tokio::test]
async fn require_works_with_a_host_address_alone() {
    let Some(server) = server() else { return };
    let (credentials, rest) = server
        .url
        .split_once("@localhost:")
        .expect("the URL must use the host localhost");
    let (port, path) = rest.split_once('/').unwrap();
    let url = format!("{credentials}@/{path}&port={port}&sslmode=require");
    assert!(ssl_in_use(&url).await.unwrap());
}

#[tokio::test]
async fn disable_connects_without_tls() {
    let Some(server) = server() else { return };
    assert!(!ssl_in_use(&server.with("sslmode=disable")).await.unwrap());
}

#[tokio::test]
async fn verify_full_accepts_the_right_ca_and_host_name() {
    let Some(server) = server() else { return };
    // channel_binding=require also proves the SCRAM exchange is bound to
    // this TLS session.
    let url = server.with(&format!(
        "sslmode=verify-full&{}&channel_binding=require",
        server.root_cert("ca.pem")
    ));
    assert!(ssl_in_use(&url).await.unwrap());
    let url = server.with(&format!("sslmode=verify-ca&{}", server.root_cert("ca.pem")));
    assert!(ssl_in_use(&url).await.unwrap());
}

#[tokio::test]
async fn verify_full_rejects_a_certificate_from_another_ca() {
    let Some(server) = server() else { return };
    for mode in ["verify-full", "verify-ca", "require"] {
        let url = server.with(&format!(
            "sslmode={mode}&{}",
            server.root_cert("wrong-ca.pem")
        ));
        assert_rejected(ssl_in_use(&url).await, "UnknownIssuer");
    }
}

#[tokio::test]
async fn verify_full_rejects_another_host_name() {
    let Some(server) = server() else { return };
    // The same server, reached by a name its certificate does not hold.
    let renamed = server.url.replacen("@localhost:", "@db.invalid:", 1);
    assert_ne!(renamed, server.url, "the URL must use the host localhost");
    let url = format!(
        "{renamed}&sslmode=verify-full&{}",
        server.root_cert("ca.pem")
    );
    assert_rejected(ssl_in_use(&url).await, "not valid for name");
    // verify-ca checks the chain only.
    let url = format!("{renamed}&sslmode=verify-ca&{}", server.root_cert("ca.pem"));
    assert!(ssl_in_use(&url).await.unwrap());
}

#[tokio::test]
async fn verify_full_without_a_root_file_checks_the_system_store() {
    let Some(server) = server() else { return };
    // The test CA is in no system store.
    assert_rejected(
        ssl_in_use(&server.with("sslmode=verify-full")).await,
        "UnknownIssuer",
    );
    assert_rejected(
        ssl_in_use(&server.with("sslrootcert=system")).await,
        "UnknownIssuer",
    );
}

#[tokio::test]
async fn a_client_certificate_authenticates_the_role_it_names() {
    let Some(server) = server() else { return };
    let url = server.cert_role(&format!(
        "sslmode=verify-full&{}&{}",
        server.root_cert("ca.pem"),
        server.client_cert()
    ));
    assert!(ssl_in_use(&url).await.unwrap());
    assert_rejected(
        ssl_in_use(&server.cert_role("sslmode=require")).await,
        "certificate",
    );
}

/// libpq's rule for the key file; Windows has no such mode bits.
#[cfg(unix)]
#[tokio::test]
async fn a_client_key_that_others_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let Some(server) = server() else { return };
    let key = std::env::temp_dir().join(format!("v12n-tls-test-{}.key", std::process::id()));
    std::fs::copy(format!("{}/client.key", server.certs), &key).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let url = server.cert_role(&format!(
        "sslmode=require&sslcert={}/client.pem&sslkey={}",
        server.certs,
        key.display()
    ));
    let result = ssl_in_use(&url).await;
    std::fs::remove_file(&key).unwrap();
    assert_rejected(result, "group or world access");
}

#[tokio::test]
async fn allow_uses_plain_text_when_the_server_accepts_it() {
    let Some(server) = server() else { return };
    assert!(!ssl_in_use(&server.with("sslmode=allow")).await.unwrap());
}

#[tokio::test]
async fn allow_retries_with_tls_when_the_server_refuses_plain_text() {
    let Some(server) = server() else { return };
    let client_cert = server.client_cert();
    // The plain attempt alone is refused by pg_hba.conf.
    assert_rejected(
        ssl_in_use(&server.cert_role(&format!("sslmode=disable&{client_cert}"))).await,
        "no encryption",
    );
    let url = server.cert_role(&format!("sslmode=allow&{client_cert}"));
    assert!(ssl_in_use(&url).await.unwrap());
}

#[tokio::test]
async fn a_crl_without_the_server_certificate_lets_it_through() {
    let Some(server) = server() else { return };
    let url = server.verified(&format!("sslcrl={}/empty.crl", server.certs));
    assert!(ssl_in_use(&url).await.unwrap());
}

#[tokio::test]
async fn a_revoked_server_certificate_is_rejected() {
    let Some(server) = server() else { return };
    let crl = format!("sslcrl={}/revoked.crl", server.certs);
    assert_rejected(ssl_in_use(&server.verified(&crl)).await, "Revoked");
    let crl_dir = format!("sslcrldir={}/revoked-crls", server.certs);
    assert_rejected(ssl_in_use(&server.verified(&crl_dir)).await, "Revoked");
    // The chain check of verify-ca, and of require with a root file,
    // applies the CRL too.
    for mode in ["verify-ca", "require"] {
        let url = server.with(&format!(
            "sslmode={mode}&{}&{crl}",
            server.root_cert("ca.pem")
        ));
        assert_rejected(ssl_in_use(&url).await, "Revoked");
    }
}

#[tokio::test]
async fn a_crl_is_required_for_every_issuer_in_the_chain() {
    let Some(server) = server() else { return };
    // A list from another CA says nothing about the server certificate.
    let url = server.verified(&format!("sslcrl={}/wrong-ca.crl", server.certs));
    assert_rejected(ssl_in_use(&url).await, "UnknownRevocationStatus");
}

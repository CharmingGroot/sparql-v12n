// SPDX-License-Identifier: Apache-2.0
//! TLS against a PostgreSQL with `ssl=on` and a server certificate for
//! `localhost` from a test CA, as `testenv/tls.sh up` starts it. Skipped
//! unless `V12N_TEST_TLS_DATABASE_URL` (a URL with the host `localhost` and
//! a query string) and `V12N_TEST_TLS_CERTS` (the directory holding
//! `ca.pem` and `wrong-ca.pem`) are set.

use v12n_core::mapping::{Mapping, MappingIndex};
use v12n_core::sql::SqlPlan;
use v12n_core::{sparql, unfold};
use v12n_postgres::{Database, Error, Limits};

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
}

fn plan() -> SqlPlan {
    let mapping = Mapping::from_turtle(SSL_IN_USE).unwrap();
    let query = sparql::parse("SELECT ?ssl WHERE { ?c <http://example.org/ssl> ?ssl }").unwrap();
    unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap()
}

/// Connects and reports whether the session uses TLS.
async fn ssl_in_use(url: &str) -> Result<bool, Error> {
    let database = Database::connect(url, Limits::default()).await?;
    let mut solutions = database.execute(&plan()).await?;
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

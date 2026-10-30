// SPDX-License-Identifier: Apache-2.0
//! Reads the certificates, the client key and the revocation lists that the
//! TLS parameters name. Every error names the file, never the URL.

use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer};
use webpki::{CertRevocationList, OwnedCertRevocationList};

use crate::Error;

fn unreadable(what: &str, path: &Path, error: &dyn std::fmt::Display) -> Error {
    Error::Database(format!(
        "cannot read the {what} {}: {error}",
        path.display()
    ))
}

fn certificates(what: &str, path: &Path) -> Result<Vec<CertificateDer<'static>>, Error> {
    let read_error = |e: &dyn std::fmt::Display| unreadable(what, path, e);
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| read_error(&e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| read_error(&e))?;
    if certs.is_empty() {
        return Err(Error::Database(format!(
            "the {what} {} holds no certificate",
            path.display()
        )));
    }
    Ok(certs)
}

/// The CAs of an `sslrootcert` file.
pub(super) fn file_roots(path: &Path) -> Result<RootCertStore, Error> {
    const WHAT: &str = "root certificate file";
    let mut roots = RootCertStore::empty();
    for cert in certificates(WHAT, path)? {
        roots.add(cert).map_err(|e| unreadable(WHAT, path, &e))?;
    }
    Ok(roots)
}

/// Read once per process: loading the store can take a while, and a
/// server opens a connection per pooled client.
pub(super) fn system_roots() -> Result<Arc<RootCertStore>, Error> {
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

/// The certificate chain of `sslcert`, the client certificate first, and
/// the private key of `sslkey`.
pub(super) fn client_identity(
    cert: &Path,
    key: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Error> {
    const WHAT: &str = "client key file";
    let chain = certificates("client certificate file", cert)?;
    check_key_access(key)?;
    let key_der = PrivateKeyDer::from_pem_file(key).map_err(|e| unreadable(WHAT, key, &e))?;
    Ok((chain, key_der))
}

/// libpq refuses a private key that others can read or change
/// (`fe-secure-openssl.c`): `u=rw` (0600) or less, or `u=rw,g=r` (0640)
/// or less when root owns the file, which is how system key stores
/// share keys with a group.
#[cfg(unix)]
fn check_key_access(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path).map_err(|e| unreadable("client key file", path, &e))?;
    if !metadata.is_file() {
        return Err(Error::Database(format!(
            "the client key file {} is not a regular file",
            path.display()
        )));
    }
    if key_mode_too_open(metadata.mode(), metadata.uid() == 0) {
        return Err(Error::Database(format!(
            "the client key file {} has group or world access; it must have \
             permissions u=rw (0600) or less, or u=rw,g=r (0640) or less if \
             owned by root",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_key_access(_path: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(any(unix, test))]
fn key_mode_too_open(mode: u32, owned_by_root: bool) -> bool {
    let forbidden = if owned_by_root { 0o037 } else { 0o077 };
    mode & forbidden != 0
}

/// The revocation lists of an `sslcrl` file.
pub(super) fn crl_file(path: &Path) -> Result<Vec<CertRevocationList<'static>>, Error> {
    let crls = crls_in(path)?;
    if crls.is_empty() {
        return Err(Error::Database(format!(
            "the CRL file {} holds no revocation list",
            path.display()
        )));
    }
    Ok(crls)
}

/// The revocation lists of an `sslcrldir` directory. OpenSSL looks up a
/// hashed directory by issuer, so it reads only files named
/// `<issuer hash>.r<n>`, as `openssl rehash` creates them; other files
/// are ignored here too.
pub(super) fn crl_dir(dir: &Path) -> Result<Vec<CertRevocationList<'static>>, Error> {
    let read_error = |e: &dyn std::fmt::Display| unreadable("CRL directory", dir, e);
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| read_error(&e))? {
        let entry = entry.map_err(|e| read_error(&e))?;
        if entry.file_name().to_str().is_some_and(is_hashed_crl_name) {
            paths.push(entry.path());
        }
    }
    if paths.is_empty() {
        return Err(Error::Database(format!(
            "the CRL directory {} holds no file named <hash>.r<n> (see openssl rehash)",
            dir.display()
        )));
    }
    paths.sort();
    let mut crls = Vec::new();
    for path in paths {
        crls.extend(crls_in(&path)?);
    }
    Ok(crls)
}

fn crls_in(path: &Path) -> Result<Vec<CertRevocationList<'static>>, Error> {
    let read_error = |e: &dyn std::fmt::Display| unreadable("CRL file", path, e);
    let mut crls = Vec::new();
    for der in CertificateRevocationListDer::pem_file_iter(path).map_err(|e| read_error(&e))? {
        let der = der.map_err(|e| read_error(&e))?;
        let crl = OwnedCertRevocationList::from_der(der.as_ref()).map_err(|e| read_error(&e))?;
        crls.push(crl.into());
    }
    Ok(crls)
}

/// `%08lx.r%d`, the name format of X509_LOOKUP_hash_dir for CRLs.
fn is_hashed_crl_name(name: &str) -> bool {
    let Some((hash, suffix)) = name.split_once('.') else {
        return false;
    };
    let Some(number) = suffix.strip_prefix('r') else {
        return false;
    };
    hash.len() == 8
        && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_access_follows_libpq() {
        for (mode, owned_by_root, too_open) in [
            (0o100600, false, false),
            (0o100400, false, false),
            (0o100640, false, true),
            (0o100604, false, true),
            (0o100640, true, false),
            (0o100660, true, true),
            (0o100644, true, true),
        ] {
            assert_eq!(
                key_mode_too_open(mode, owned_by_root),
                too_open,
                "{mode:o}, owned by root: {owned_by_root}"
            );
        }
    }

    #[test]
    fn only_hashed_names_are_read_from_a_crl_directory() {
        for name in ["d7eed2ab.r0", "0123abcd.r12"] {
            assert!(is_hashed_crl_name(name), "{name}");
        }
        for name in [
            "d7eed2ab.0",
            "D7EED2AB.r0",
            "d7eed2a.r0",
            "d7eed2ab.r",
            "d7eed2ab.r0.bak",
            "revoked.crl",
        ] {
            assert!(!is_hashed_crl_name(name), "{name}");
        }
    }

    #[test]
    fn missing_files_are_reported_with_their_path() {
        let missing = Path::new("/nonexistent/v12n/file.pem");
        let errors = [
            file_roots(missing).map(drop),
            crl_file(missing).map(drop),
            crl_dir(missing).map(drop),
            client_identity(missing, missing).map(drop),
        ];
        for error in errors {
            let error = error.unwrap_err();
            assert!(matches!(error, Error::Database(_)), "{error}");
            assert!(error.to_string().contains("/nonexistent/v12n/"), "{error}");
        }
    }
}

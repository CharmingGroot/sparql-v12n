// SPDX-License-Identifier: Apache-2.0
//! The server certificate check of `verify-ca` and `verify-full`, with the
//! revocation lists of `sslcrl` and `sslcrldir`.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::verify_server_name;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, SignatureVerificationAlgorithm, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{CertificateError, DigitallySignedStruct, OtherError, RootCertStore, SignatureScheme};
use webpki::{
    CertRevocationList, EndEntityCert, ExpirationPolicy, KeyUsage, RevocationCheckDepth,
    RevocationOptions, RevocationOptionsBuilder, UnknownStatusPolicy,
};

/// What a server certificate must satisfy.
#[derive(Debug)]
pub(super) struct Trust {
    pub(super) roots: Arc<RootCertStore>,
    /// Empty when no revocation list is given.
    pub(super) crls: Vec<CertRevocationList<'static>>,
    /// `verify-full`: the certificate must name the host.
    pub(super) check_name: bool,
}

impl Trust {
    fn verify(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        now: UnixTime,
        algorithms: &[&dyn SignatureVerificationAlgorithm],
    ) -> Result<(), rustls::Error> {
        let cert = EndEntityCert::try_from(end_entity).map_err(chain_error)?;
        let crls: Vec<&CertRevocationList<'_>> = self.crls.iter().collect();
        cert.verify_for_usage(
            algorithms,
            &self.roots.roots,
            intermediates,
            now,
            KeyUsage::server_auth(),
            revocation(&crls),
            None,
        )
        .map_err(chain_error)?;
        if self.check_name {
            verify_server_name(&ParsedCertificate::try_from(end_entity)?, server_name)?;
        }
        Ok(())
    }
}

/// libpq sets `X509_V_FLAG_CRL_CHECK | X509_V_FLAG_CRL_CHECK_ALL`
/// (`fe-secure-openssl.c`), so OpenSSL wants a CRL for every certificate
/// below the trust anchor and fails without one; it also rejects a CRL
/// past its next update.
fn revocation<'a>(crls: &'a [&'a CertRevocationList<'a>]) -> Option<RevocationOptions<'a>> {
    let builder = RevocationOptionsBuilder::new(crls).ok()?;
    Some(
        builder
            .with_depth(RevocationCheckDepth::Chain)
            .with_status_policy(UnknownStatusPolicy::Deny)
            .with_expiration_policy(ExpirationPolicy::Enforce)
            .build(),
    )
}

/// The rustls form of a chain error, as rustls's own verifier reports it,
/// so that messages read `invalid peer certificate: UnknownIssuer`.
fn chain_error(error: webpki::Error) -> rustls::Error {
    use webpki::Error as E;
    let error = match error {
        E::UnknownIssuer => CertificateError::UnknownIssuer,
        E::CertRevoked => CertificateError::Revoked,
        E::UnknownRevocationStatus => CertificateError::UnknownRevocationStatus,
        E::CertExpired { time, not_after } => CertificateError::ExpiredContext { time, not_after },
        E::CertNotValidYet { time, not_before } => {
            CertificateError::NotValidYetContext { time, not_before }
        }
        E::CrlExpired { time, next_update } => {
            CertificateError::ExpiredRevocationListContext { time, next_update }
        }
        E::InvalidSignatureForPublicKey => CertificateError::BadSignature,
        other => CertificateError::Other(OtherError(Arc::new(other))),
    };
    error.into()
}

/// Checks the server certificate as `sslmode` asks. Without a [`Trust`] it
/// accepts any certificate, as libpq does in `prefer` and `require`; the
/// handshake signatures are checked in every mode.
#[derive(Debug)]
pub(super) struct Verifier {
    pub(super) trust: Option<Trust>,
    pub(super) algorithms: WebPkiSupportedAlgorithms,
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
        if let Some(trust) = &self.trust {
            trust.verify(
                end_entity,
                intermediates,
                server_name,
                now,
                self.algorithms.all,
            )?;
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

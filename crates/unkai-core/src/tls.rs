//! Shared TLS plumbing for `unkai-imap` and `unkai-smtp`.
//!
//! Both protocol crates use rustls under the hood, so we keep the
//! cert-validation logic in one place. The two surfaces:
//!
//! - [`build_client_config`] — produces a `rustls::ClientConfig`
//!   pre-loaded with Mozilla's webpki-roots *plus* whatever extra
//!   certs the user has explicitly trusted for this account.
//!   `unkai-imap` hands this straight to `tokio-rustls`.
//!
//! - [`extra_root_der`] — same trusted-cert DER bytes, but flat,
//!   for callers (notably lettre) that take roots one at a time
//!   via their own builder API.
//!
//! - [`fingerprint_sha256`] — formats SHA-256(DER) as
//!   `aa:bb:cc:dd:…` for the "trust this server?" prompt and for
//!   the audit list in account settings.
//!
//! The "no-verify" probing path (used to capture a self-signed
//! cert before the user has seen it) lives in `unkai-imap` and
//! `unkai-smtp` because it's runtime-coupled. We expose
//! [`NoVerifier`] here so they don't each invent one.

use std::sync::Arc;

use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::models::TrustedCert;

/// Build a `rustls::ClientConfig` for an account that may have its
/// own list of pre-trusted certs.
///
/// - **No trusted certs** → standard webpki-roots verification.
///   The cheap, common path; same behaviour every other client
///   gets out of the box.
/// - **Trusted certs present** → custom [`FingerprintVerifier`]
///   that first delegates to the standard webpki verifier, and
///   on failure falls back to comparing the SHA-256 of the leaf
///   cert against the user's trust list. This sidesteps rustls's
///   `RootCertStore::add`, which validates each entry as a proper
///   CA trust anchor and rejects self-signed leaves (the most
///   common reason a user would land in the trust prompt in the
///   first place).
pub fn build_client_config(extra_roots: &[TrustedCert]) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    if extra_roots.is_empty() {
        debug!("TLS client config: webpki-roots only (no per-account trusted certs)");
        return Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
    }

    let fingerprints: Vec<String> = extra_roots
        .iter()
        // Normalise stored fingerprints once at config-build time:
        // lowercased, separator-stripped. The verifier compares
        // against this canonical form so a fingerprint stored with
        // colons / uppercase from a previous build still matches a
        // freshly-computed one (or vice versa). Belt-and-braces
        // against accidental format drift across versions.
        .map(|c| canonicalize_fingerprint(&c.sha256))
        .collect();
    debug!(
        "TLS client config: {} trusted fingerprint(s): {:?}",
        fingerprints.len(),
        fingerprints
    );
    let inner = WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .expect("webpki-roots verifier build");
    let verifier = Arc::new(FingerprintVerifier {
        inner,
        trusted_fingerprints: fingerprints,
    });
    Arc::new(
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    )
}

/// Strip the `:` separators and lowercase a SHA-256 fingerprint
/// string so trust comparisons are insensitive to formatting drift
/// between the prompt UI, the trust-list write path, and the
/// verifier. `aa:bb:CC` and `AABBCC` both end up as `aabbcc`.
fn canonicalize_fingerprint(fp: &str) -> String {
    fp.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Custom rustls verifier that accepts a cert if **either**:
///   1. The standard webpki chain validates against Mozilla's
///      curated roots — i.e. a normal CA-signed cert. Hostname
///      matching, expiry, signature: all the usual checks. Or
///   2. The leaf cert's SHA-256 matches a user-trusted fingerprint
///      from the account's `trusted_certs` list — the "I know this
///      is my self-signed mail server" escape hatch.
///
/// Signature verification (TLS 1.2 / 1.3 handshake signing) is
/// always delegated to the inner webpki verifier. We only override
/// the *trust* decision, not the cryptographic checks.
#[derive(Debug)]
struct FingerprintVerifier {
    inner: Arc<WebPkiServerVerifier>,
    trusted_fingerprints: Vec<String>,
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(v) => Ok(v),
            Err(webpki_err) => {
                // Walk the entire presented chain — leaf and every
                // intermediate. Some servers send the chain in the
                // order specified by RFC 5246 (leaf first); others
                // reorder it; and the user's "trust this" prompt
                // captures only one DER blob. Comparing every cert
                // in the chain against the trusted fingerprints
                // means a reorder doesn't cause a false rejection.
                let leaf_fp = canonicalize_fingerprint(&fingerprint_sha256(end_entity.as_ref()));
                let intermediate_fps: Vec<String> = intermediates
                    .iter()
                    .map(|c| canonicalize_fingerprint(&fingerprint_sha256(c.as_ref())))
                    .collect();
                let chain_fps: Vec<&String> = std::iter::once(&leaf_fp)
                    .chain(intermediate_fps.iter())
                    .collect();

                let matched = chain_fps
                    .iter()
                    .any(|fp| self.trusted_fingerprints.iter().any(|t| t == *fp));
                if matched {
                    Ok(ServerCertVerified::assertion())
                } else {
                    warn!(
                        "TLS verify rejected: webpki={webpki_err}, leaf={leaf_fp}, \
                         intermediates={intermediate_fps:?}, trusted={:?}",
                        self.trusted_fingerprints
                    );
                    Err(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::UnknownIssuer,
                    ))
                }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// SHA-256 of the DER bytes, formatted as `aa:bb:cc:…` lowercase
/// hex. Stable representation for the trust prompt and the
/// per-account "trusted certs" list in settings.
pub fn fingerprint_sha256(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    let hex = hex::encode(digest);
    let mut out = String::with_capacity(hex.len() + hex.len() / 2);
    for (i, c) in hex.chars().enumerate() {
        if i > 0 && i % 2 == 0 {
            out.push(':');
        }
        out.push(c);
    }
    out
}

/// "Trust everything" rustls verifier. Used **only** by the
/// probing connect path that captures a server's leaf cert so the
/// user can decide whether to trust it. Never returned from a
/// production code path that handles real mail.
#[derive(Debug)]
pub struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

/// Build a "no-verify" rustls config. Same warning as `NoVerifier`
/// — only the cert-probe path should ever get one.
pub fn no_verify_config() -> Arc<ClientConfig> {
    Arc::new(
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_format() {
        let fp = fingerprint_sha256(b"hello");
        // Pre-computed so the test catches accidental format drift.
        assert_eq!(
            fp,
            "2c:f2:4d:ba:5f:b0:a3:0e:26:e8:3b:2a:c5:b9:e2:9e:1b:16:1e:5c:1f:a7:42:5e:73:04:33:62:93:8b:98:24"
        );
    }
}

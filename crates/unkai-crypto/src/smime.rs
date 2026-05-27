//! S/MIME (RFC 8551 / RFC 5652 CMS) sign / verify / encrypt / decrypt
//! operations on X.509 certificate chains.
//!
//! This module is the X.509-based counterpart to [`crate::ops`]'s
//! OpenPGP primitives.  Public surface, error shape, and signature-
//! status semantics mirror that module deliberately — the cache layer
//! and the Tauri IPC boundary should be able to handle either stack
//! through the same enum (`Protection`, `SignatureStatus`) and the
//! same general "armored bytes in, status enum out" calling
//! convention.
//!
//! ## What this module does NOT do
//!
//! - **MIME wrapping.** RFC 8551 says "wrap the CMS bytes in
//!   `application/pkcs7-mime; smime-type=enveloped-data`" (or the
//!   `multipart/signed` shape for sign-only).  Building that MIME
//!   wrapper belongs with the SMTP send path and the IMAP receive
//!   parser, which already own MIME assembly.  This module hands
//!   back **raw DER CMS bytes**; the protocol crates wrap them.
//! - **CA chain validation.**  The first integration chunk only
//!   verifies the cryptographic signature against a caller-supplied
//!   list of trusted certificates (the same TOFU shape we use for
//!   PGP fingerprints).  Real CA / `webpki-roots` integration is a
//!   later #338 sub-chunk.
//! - **`.p7s` vs `.p7m` distinction at this layer.**  Detached vs
//!   opaque signing is the MIME wrapper's concern.  We always emit
//!   detached signatures from [`smime_sign`] (matching the
//!   `multipart/signed` shape RFC 8551 §3.4 prefers) and always
//!   produce `EnvelopedData` from [`smime_encrypt`] (the
//!   `pkcs7-mime; smime-type=enveloped-data` shape).
//!
//! ## Algorithm choices (RFC 8551 §2.7)
//!
//! - Content encryption: **AES-256-CBC**.  RFC 8551 §2.7.1 mandates
//!   AES-128-CBC at minimum and recommends AES-256-CBC; we pick the
//!   stronger one to match the PGP layer's AES-256.
//! - Key transport: **RSA** (via the recipient's RSA public key in
//!   their X.509 cert).  RFC 8551 §2.7 mandates RSA at minimum.
//! - Digest: **SHA-256**.  RFC 8551 §2.7.2 mandates SHA-256 at
//!   minimum, matches our PGP layer's choice.
//!
//! When EC certs become more common in the wild we can extend the
//! cert wrapper to dispatch on key algorithm; for now we lean on
//! OpenSSL's internal cipher selection (it picks the right key
//! transport for the recipient cert's algorithm automatically).
//!
//! See [issue #338](https://github.com/firn-labs/unkai-mail/issues/338).

use openssl::cms::{CMSOptions, CmsContentInfo};
use openssl::hash::MessageDigest;
use openssl::pkcs12::Pkcs12;
use openssl::pkey::{PKey, Private};
use openssl::stack::Stack;
use openssl::symm::Cipher;
use openssl::x509::{X509, X509NameRef};
use unkai_core::UnkaiError;

use crate::types::SignatureStatus;

/// Result of decrypting and (eventually — see module docs) verifying an
/// S/MIME message in one pass.
///
/// Mirrors [`crate::ops::DecryptedMessage`] so callers can treat the two
/// stacks uniformly.  `signature_status` is currently always `None` for
/// S/MIME — the nested "encrypted SignedData inside EnvelopedData" path
/// (RFC 8551 §3.6) is a follow-up sub-chunk; today encrypt-and-sign go
/// through the wire as two separate operations the caller composes.
#[derive(Debug, Clone)]
pub struct DecryptedSmimeMessage {
    /// The recovered plaintext bytes.  For an S/MIME `application/pkcs7-mime`
    /// envelope, this is the full inner MIME body that the IMAP receive
    /// path will hand back to `mail-parser`.
    pub plaintext: Vec<u8>,

    /// Outcome of verifying an inner signature, if any.  Always `None` in
    /// the current chunk — the nested sign-then-encrypt path lands later.
    pub signature_status: Option<SignatureStatus>,

    /// Subject distinguished name of the signing cert, when one was
    /// embedded and we could attribute it.  Always `None` in the current
    /// chunk for the same reason as `signature_status`.
    pub signer_subject_dn: Option<String>,
}

/// A recipient's (or our own) X.509 certificate.
///
/// Wraps OpenSSL's [`X509`] to keep the rest of the workspace from
/// depending on the `openssl` crate directly — every other crate
/// touches certificates through this façade so we can swap the
/// implementation later without churning callers.
#[derive(Debug, Clone)]
pub struct Certificate {
    inner: X509,
}

impl Certificate {
    /// SHA-256 fingerprint of the DER-encoded certificate, uppercase
    /// hex with colons (the form `openssl x509 -fingerprint -sha256`
    /// emits and most OS keychains display).  Used as the cache key
    /// in the future `smime_certs` table and as the human-readable
    /// identifier shown under each contact.
    pub fn fingerprint(&self) -> String {
        // `digest` on an X509 hashes the DER encoding — exactly the
        // semantics every X.509 tool uses for the displayed fingerprint.
        match self.inner.digest(MessageDigest::sha256()) {
            Ok(bytes) => bytes
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(":"),
            // OpenSSL refusing to digest a parsed X509 is so unusual
            // (the bytes already round-tripped through ASN.1) that
            // we don't propagate it through this accessor — the worst
            // case is the UI showing "unknown" rather than crashing.
            Err(_) => "unknown".to_string(),
        }
    }

    /// Subject distinguished name as a single human-readable string,
    /// e.g. `"/CN=Alice Example/O=Example Corp"`.  Used in MailView's
    /// "signed by …" chip for S/MIME messages where the subject is
    /// what identifies the signer.
    pub fn subject_dn(&self) -> String {
        x509_name_to_string(self.inner.subject_name())
    }

    /// Issuer distinguished name in the same shape as [`Self::subject_dn`].
    /// Useful for the trust-model UI later (showing which CA issued the
    /// cert) and for the SignatureStatus chip's "issued by …" tooltip.
    pub fn issuer_dn(&self) -> String {
        x509_name_to_string(self.inner.issuer_name())
    }

    /// Primary email address from the Subject Alternative Name extension
    /// (`rfc822Name`).  RFC 8551 §3 mandates that an S/MIME signing /
    /// encrypting cert bind its email through SAN rather than the
    /// historical CN-as-email trick, so we read SAN.
    ///
    /// Returns the first SAN email entry; certs with multiple SAN emails
    /// (rare in practice) will need the wiring layer to fetch all of them
    /// via a future accessor.
    pub fn email(&self) -> Option<String> {
        self.inner
            .subject_alt_names()?
            .iter()
            .find_map(|name| name.email().map(|s| s.to_string()))
    }

    /// DER-encoded certificate bytes.  Used by the storage layer when
    /// persisting a cert into the future `smime_certs` table — we keep
    /// the canonical wire form so round-tripping through the database
    /// can't drift the fingerprint.
    pub fn to_der(&self) -> Result<Vec<u8>, UnkaiError> {
        self.inner
            .to_der()
            .map_err(|e| UnkaiError::Crypto(format!("Failed to serialize cert to DER: {e}")))
    }
}

/// Our own certificate paired with its private key — what `smime_sign`
/// and `smime_decrypt` need to perform private-key operations.
///
/// `chain` carries any intermediate CA certs that came alongside the
/// leaf in a `.p12` import.  We include them in the emitted CMS
/// `SignedData` so recipients can chain-validate without having to
/// fetch our issuer's cert out-of-band.
pub struct CertificateWithKey {
    leaf: X509,
    private_key: PKey<Private>,
    chain: Vec<X509>,
}

impl std::fmt::Debug for CertificateWithKey {
    /// Manual `Debug` impl that elides the private key.  Auto-derive
    /// would risk leaking key material through `tracing::debug!`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateWithKey")
            .field("leaf_fingerprint", &self.leaf_certificate().fingerprint())
            .field("chain_len", &self.chain.len())
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl CertificateWithKey {
    /// A read-only view of the leaf cert — used wherever a caller has
    /// a `CertificateWithKey` and needs the public-side accessors
    /// (fingerprint, subject, email) without re-implementing them.
    pub fn leaf_certificate(&self) -> Certificate {
        Certificate {
            inner: self.leaf.clone(),
        }
    }

    /// SHA-256 fingerprint of the leaf cert.  Pairs with stored
    /// metadata in the UI exactly like [`crate::keys::PrivateKey::fingerprint`].
    pub fn fingerprint(&self) -> String {
        self.leaf_certificate().fingerprint()
    }
}

/// Parse a PEM-encoded X.509 certificate (the `-----BEGIN CERTIFICATE-----`
/// form most CAs hand out and most mail clients copy/paste).
pub fn parse_pem_cert(bytes: &[u8]) -> Result<Certificate, UnkaiError> {
    let inner = X509::from_pem(bytes)
        .map_err(|e| UnkaiError::Crypto(format!("Failed to parse PEM certificate: {e}")))?;
    Ok(Certificate { inner })
}

/// Parse a DER-encoded X.509 certificate (the binary form stored in
/// `.cer` / `.crt` files and embedded in CMS messages).
pub fn parse_der_cert(bytes: &[u8]) -> Result<Certificate, UnkaiError> {
    let inner = X509::from_der(bytes)
        .map_err(|e| UnkaiError::Crypto(format!("Failed to parse DER certificate: {e}")))?;
    Ok(Certificate { inner })
}

/// Parse a PKCS#12 (`.p12` / `.pfx`) envelope: the canonical bundle
/// that ships an S/MIME identity (leaf cert + private key + optional
/// CA chain), passphrase-encrypted.
///
/// Wrong-passphrase errors are mapped to a single user-friendly
/// `"Wrong PKCS#12 passphrase"` sentence so the UI can branch on it
/// without scraping OpenSSL's underlying message.  Any other parse
/// failure passes the OpenSSL error through verbatim in the
/// `UnkaiError::Crypto` body so logs stay useful.
pub fn parse_pkcs12(bytes: &[u8], passphrase: &str) -> Result<CertificateWithKey, UnkaiError> {
    let p12 = Pkcs12::from_der(bytes)
        .map_err(|e| UnkaiError::Crypto(format!("Failed to parse PKCS#12 envelope: {e}")))?;
    let parsed = p12.parse2(passphrase).map_err(|e| {
        // OpenSSL surfaces a wrong PKCS#12 passphrase as one of several
        // related errors depending on the encryption mode the file uses
        // ("mac verify failure", "decrypt error", "wrong final block
        // length", …).  Collapse the common ones into a single sentence
        // the user can act on; anything else falls through with the
        // raw message so logs stay diagnostic.
        let raw = format!("{e}").to_lowercase();
        if raw.contains("mac verify") || raw.contains("decrypt") {
            UnkaiError::Crypto("Wrong PKCS#12 passphrase".into())
        } else {
            UnkaiError::Crypto(format!("Failed to parse PKCS#12 envelope: {e}"))
        }
    })?;

    let leaf = parsed.cert.ok_or_else(|| {
        UnkaiError::Crypto("PKCS#12 envelope is missing the leaf certificate".into())
    })?;
    let private_key = parsed
        .pkey
        .ok_or_else(|| UnkaiError::Crypto("PKCS#12 envelope is missing the private key".into()))?;
    let chain: Vec<X509> = parsed
        .ca
        .map(|stack| stack.into_iter().collect())
        .unwrap_or_default();

    Ok(CertificateWithKey {
        leaf,
        private_key,
        chain,
    })
}

/// Produce a CMS `SignedData` *detached signature* over `payload`,
/// returned as DER bytes.  The wire shape is the inner half of
/// `multipart/signed; protocol="application/pkcs7-signature"` from
/// RFC 8551 §3.4 — the SMTP send path will base64-wrap this DER blob
/// into an `application/pkcs7-signature` MIME part.
///
/// The signer's leaf cert is embedded in the SignedData (per
/// RFC 5652 §5.1's `certificates` field convention) so a recipient
/// can identify us by the cert without having to fetch it out of
/// band.  Any intermediate CAs in `signer.chain` go along for the
/// ride.
pub fn smime_sign(payload: &[u8], signer: &CertificateWithKey) -> Result<Vec<u8>, UnkaiError> {
    let chain_stack = certs_to_stack(&signer.chain)?;
    // `DETACHED` produces the signature-only form (payload not embedded
    // in the CMS message).  `BINARY` skips OpenSSL's internal CR-LF
    // canonicalisation — the SMTP layer will own canonicalisation when
    // it wraps this signature into the outer multipart/signed envelope,
    // exactly like the PGP/MIME sign-only path does (see
    // `unkai_smtp::client::canonicalize_for_pgp_signing`).
    let cms = CmsContentInfo::sign(
        Some(&signer.leaf),
        Some(&signer.private_key),
        Some(&chain_stack),
        Some(payload),
        CMSOptions::DETACHED | CMSOptions::BINARY,
    )
    .map_err(|e| UnkaiError::Crypto(format!("CMS sign failed: {e}")))?;
    cms.to_der()
        .map_err(|e| UnkaiError::Crypto(format!("Failed to serialize CMS SignedData: {e}")))
}

/// Encrypt `plaintext` to one or more recipient certificates.  No
/// signature.  Returns CMS `EnvelopedData` DER bytes — the inner
/// half of `application/pkcs7-mime; smime-type=enveloped-data` per
/// RFC 8551 §3.2.
pub fn smime_encrypt(plaintext: &[u8], recipients: &[&Certificate]) -> Result<Vec<u8>, UnkaiError> {
    if recipients.is_empty() {
        return Err(UnkaiError::Crypto(
            "Cannot encrypt: no recipient certificates supplied".into(),
        ));
    }

    // OpenSSL takes a `Stack<X509>` of recipients; we clone each X509
    // because `Stack::push` consumes by value and a `&Certificate`
    // doesn't expose the inner `X509` directly.  X509 is reference-counted
    // inside OpenSSL so cloning is cheap.
    let mut stack: Stack<X509> = Stack::new()
        .map_err(|e| UnkaiError::Crypto(format!("Failed to create recipient stack: {e}")))?;
    for cert in recipients {
        stack
            .push(cert.inner.clone())
            .map_err(|e| UnkaiError::Crypto(format!("Failed to push recipient cert: {e}")))?;
    }

    // AES-256-CBC is the RFC 8551 §2.7.1 SHOULD set — we match the
    // PGP layer's AES-256 here so both stacks use the same content
    // cipher strength.
    let cipher = Cipher::aes_256_cbc();
    let cms = CmsContentInfo::encrypt(&stack, plaintext, cipher, CMSOptions::BINARY)
        .map_err(|e| UnkaiError::Crypto(format!("CMS encrypt failed: {e}")))?;

    cms.to_der()
        .map_err(|e| UnkaiError::Crypto(format!("Failed to serialize CMS EnvelopedData: {e}")))
}

/// Decrypt a CMS `EnvelopedData` blob (DER) using the supplied
/// certificate + private key.  Mirrors [`crate::ops::decrypt_and_verify`]'s
/// shape — `signature_status` is `None` in this chunk; the nested
/// `SignedData`-inside-`EnvelopedData` path lands in a later sub-chunk.
pub fn smime_decrypt(
    ciphertext: &[u8],
    decrypt_key: &CertificateWithKey,
) -> Result<DecryptedSmimeMessage, UnkaiError> {
    let cms = CmsContentInfo::from_der(ciphertext)
        .map_err(|e| UnkaiError::Crypto(format!("Failed to parse CMS message: {e}")))?;
    let plaintext = cms
        .decrypt(&decrypt_key.private_key, &decrypt_key.leaf)
        .map_err(|e| {
            // The most common cause of CMS decrypt failing in practice
            // is "this message wasn't encrypted to my cert" — the user
            // has the cert but isn't on the recipient list (forwarded
            // mail, mis-directed envelope).  Surface a sentence the UI
            // can act on rather than dumping OpenSSL's stack.
            let raw = format!("{e}").to_lowercase();
            if raw.contains("no recipient") || raw.contains("recipient") {
                UnkaiError::Crypto(
                    "Decryption failed — this message wasn't encrypted to your certificate".into(),
                )
            } else {
                UnkaiError::Crypto(format!("CMS decrypt failed: {e}"))
            }
        })?;

    Ok(DecryptedSmimeMessage {
        plaintext,
        signature_status: None,
        signer_subject_dn: None,
    })
}

/// Verify a CMS `SignedData` detached signature over `payload` against
/// a list of trusted certificates.  Returns:
///
/// - [`SignatureStatus::Valid`] — the signature is cryptographically
///   sound *and* the signer's cert matches one in `trusted_certs` by
///   SHA-256 fingerprint (`NOINTERN` flag forces OpenSSL to only
///   consider the certs we hand it, ignoring the cert embedded in the
///   SignedData itself).
/// - [`SignatureStatus::UnknownSigner`] — signature is well-formed but
///   either the math doesn't match any supplied trusted cert or no
///   matching cert was in the list.  We can't currently distinguish
///   "wrong cert" from "tampered data" through OpenSSL's binding, so
///   both collapse to this status — same conservative behaviour as
///   [`crate::ops::verify_detached`] for OpenPGP.
///
/// We pair `NOINTERN` (only trust certs the caller hands us, never the
/// embedded one) with `NO_SIGNER_CERT_VERIFY` (skip CA chain
/// validation — the trust-model chunk later will replace this with
/// real chain checking against an `X509Store`).
pub fn smime_verify(
    payload: &[u8],
    signature: &[u8],
    trusted_certs: &[&Certificate],
) -> Result<SignatureStatus, UnkaiError> {
    let mut cms = CmsContentInfo::from_der(signature)
        .map_err(|e| UnkaiError::Crypto(format!("Failed to parse CMS signature: {e}")))?;

    let mut trust_stack: Stack<X509> = Stack::new()
        .map_err(|e| UnkaiError::Crypto(format!("Failed to create trust stack: {e}")))?;
    for cert in trusted_certs {
        trust_stack
            .push(cert.inner.clone())
            .map_err(|e| UnkaiError::Crypto(format!("Failed to push trusted cert: {e}")))?;
    }

    let flags = CMSOptions::NOINTERN
        | CMSOptions::NO_SIGNER_CERT_VERIFY
        | CMSOptions::DETACHED
        | CMSOptions::BINARY;

    match cms.verify(Some(&trust_stack), None, Some(payload), None, flags) {
        Ok(()) => Ok(SignatureStatus::Valid),
        Err(_) => Ok(SignatureStatus::UnknownSigner),
    }
}

/// Collect a slice of X.509 certificates into an OpenSSL `Stack<X509>`.
/// Used in both the sign and verify paths; factored out so the cloning
/// boilerplate (X509 doesn't impl `Copy` and `Stack::push` consumes by
/// value) lives in one place.
fn certs_to_stack(certs: &[X509]) -> Result<Stack<X509>, UnkaiError> {
    let mut stack: Stack<X509> = Stack::new()
        .map_err(|e| UnkaiError::Crypto(format!("Failed to create cert stack: {e}")))?;
    for cert in certs {
        stack
            .push(cert.clone())
            .map_err(|e| UnkaiError::Crypto(format!("Failed to push cert: {e}")))?;
    }
    Ok(stack)
}

/// Build a `/CN=…/O=…/`-style string from an X.509 name.  OpenSSL has
/// no public `oneline()` binding for X509Name in the Rust crate, so we
/// walk the entries and assemble the OID-short-name + value pairs
/// ourselves.  Non-UTF-8 entry values fall back to `?` rather than
/// failing the whole call — a malformed DN should still surface a
/// best-effort string in the UI rather than crash.
fn x509_name_to_string(name: &X509NameRef) -> String {
    let mut out = String::new();
    for entry in name.entries() {
        let short_name = entry.object().nid().short_name().unwrap_or("?");
        let value = entry
            .data()
            .as_utf8()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "?".to_string());
        out.push('/');
        out.push_str(short_name);
        out.push('=');
        out.push_str(&value);
    }
    if out.is_empty() { "/".to_string() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::rsa::Rsa;
    use openssl::x509::extension::SubjectAlternativeName;
    use openssl::x509::{X509Builder, X509NameBuilder};

    /// Generate a self-signed RSA-2048 cert with the supplied common name
    /// and binding email, valid for one year.  Mirrors the PGP test
    /// helper `make_test_keypair` — slow (~1s for RSA generation) but
    /// gives each test a fresh independent identity.
    ///
    /// We deliberately keep the cert self-signed rather than wiring up a
    /// test CA: this first chunk's `smime_verify` uses fingerprint-based
    /// trust (`NOINTERN`), so chain construction isn't exercised.  When
    /// the trust-model sub-chunk lands it'll bring its own test CA
    /// fixture for the chain-verify path.
    fn make_test_cert(common_name: &str, email: &str) -> CertificateWithKey {
        let rsa = Rsa::generate(2048).expect("test: generate RSA-2048");
        let pkey = PKey::from_rsa(rsa).expect("test: wrap RSA as PKey");

        let mut name_builder = X509NameBuilder::new().expect("test: name builder");
        name_builder
            .append_entry_by_text("CN", common_name)
            .expect("test: set CN");
        name_builder
            .append_entry_by_text("O", "Test")
            .expect("test: set O");
        let name = name_builder.build();

        let mut cert_builder = X509Builder::new().expect("test: cert builder");
        cert_builder.set_version(2).expect("test: set v3");

        // Serial is arbitrary for self-signed test certs; just needs to be
        // a positive integer (RFC 5280 §4.1.2.2).
        let serial = BigNum::from_u32(1)
            .expect("test: serial bn")
            .to_asn1_integer()
            .expect("test: serial asn1");
        cert_builder
            .set_serial_number(&serial)
            .expect("test: set serial");

        cert_builder.set_subject_name(&name).expect("test: subject");
        cert_builder
            .set_issuer_name(&name)
            .expect("test: issuer (self-signed)");
        cert_builder.set_pubkey(&pkey).expect("test: set pubkey");

        let not_before = Asn1Time::days_from_now(0).expect("test: not_before");
        let not_after = Asn1Time::days_from_now(365).expect("test: not_after");
        cert_builder
            .set_not_before(&not_before)
            .expect("test: set not_before");
        cert_builder
            .set_not_after(&not_after)
            .expect("test: set not_after");

        // SAN with the email — RFC 8551 §3 requires the binding email
        // to live in SAN rfc822Name, not CN.  Our `Certificate::email`
        // accessor walks SAN, so the test exercises that path.
        let san = SubjectAlternativeName::new()
            .email(email)
            .build(&cert_builder.x509v3_context(None, None))
            .expect("test: build SAN");
        cert_builder
            .append_extension(san)
            .expect("test: append SAN");

        cert_builder
            .sign(&pkey, MessageDigest::sha256())
            .expect("test: self-sign");
        let cert = cert_builder.build();

        CertificateWithKey {
            leaf: cert,
            private_key: pkey,
            chain: vec![],
        }
    }

    /// Re-derive the public-side view from a [`CertificateWithKey`] by
    /// round-tripping the leaf through DER — mirrors the PGP test
    /// helper `public_view`, exercising the same `parse_der_cert` path
    /// the real app uses when a recipient's cert arrives over CardDAV
    /// or via a `.cer` paste.
    fn public_view(private: &CertificateWithKey) -> Certificate {
        let der = private.leaf.to_der().expect("test: serialize leaf to DER");
        parse_der_cert(&der).expect("test: re-parse leaf cert")
    }

    #[test]
    fn encrypt_then_decrypt_round_trip() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let alice_cert = public_view(&alice);
        let plaintext = b"top secret memo";

        let ciphertext = smime_encrypt(plaintext, &[&alice_cert]).expect("encrypt");
        let decrypted = smime_decrypt(&ciphertext, &alice).expect("decrypt");

        assert_eq!(decrypted.plaintext, plaintext);
        assert_eq!(
            decrypted.signature_status, None,
            "encrypt-only carries no sig"
        );
        assert_eq!(decrypted.signer_subject_dn, None);
    }

    #[test]
    fn encrypt_with_no_recipients_is_an_error() {
        let err = smime_encrypt(b"x", &[]).expect_err("must reject");
        assert!(
            matches!(err, UnkaiError::Crypto(ref m) if m.contains("no recipient")),
            "got: {err:?}"
        );
    }

    #[test]
    fn sign_then_verify_round_trip() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let alice_cert = public_view(&alice);
        let payload = b"hello, world";

        let sig = smime_sign(payload, &alice).expect("sign");
        let status = smime_verify(payload, &sig, &[&alice_cert]).expect("verify");

        assert_eq!(status, SignatureStatus::Valid);
    }

    #[test]
    fn verify_reports_unknown_signer_when_no_trusted_cert_matches() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let bob = make_test_cert("Bob Example", "bob@example.com");
        let bob_cert = public_view(&bob);
        let payload = b"signed by alice";

        let sig = smime_sign(payload, &alice).expect("sign");
        // We hand the verifier only Bob's cert.  Alice's signature can't
        // be attributed; status falls back to UnknownSigner — same
        // semantics as the PGP `verify_detached` test.
        let status = smime_verify(payload, &sig, &[&bob_cert]).expect("verify");

        assert_eq!(status, SignatureStatus::UnknownSigner);
    }

    #[test]
    fn verify_reports_unknown_signer_when_payload_is_tampered() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let alice_cert = public_view(&alice);
        let payload = b"original payload";
        let tampered = b"TAMPERED payload";

        let sig = smime_sign(payload, &alice).expect("sign");
        // OpenSSL's verify returns Err for both "wrong signer" and
        // "tampered data" without an easy way to distinguish — we
        // collapse both to UnknownSigner, matching the conservative
        // behaviour of the PGP verify helper.
        let status = smime_verify(tampered, &sig, &[&alice_cert]).expect("verify");

        assert_eq!(status, SignatureStatus::UnknownSigner);
    }

    #[test]
    fn certificate_fingerprint_is_sha256_with_colons() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let cert = public_view(&alice);
        let fp = cert.fingerprint();

        // SHA-256 = 32 bytes = 32 two-char hex groups joined by 31 colons
        // = 64 + 31 = 95 chars.  Spot-checking the shape catches the
        // common mistakes (no colons, lowercase, wrong digest).
        assert_eq!(fp.len(), 95, "fingerprint should be 95 chars, got {fp:?}");
        assert_eq!(
            fp.matches(':').count(),
            31,
            "fingerprint should have 31 colons"
        );
        assert!(
            fp.chars().all(|c| c.is_ascii_hexdigit() || c == ':'),
            "fingerprint should be uppercase hex with colons"
        );
        assert!(
            fp.chars()
                .filter(|c| c.is_ascii_alphabetic())
                .all(|c| c.is_ascii_uppercase()),
            "fingerprint hex digits should be uppercase"
        );

        // Fingerprint is stable across two derivations of the same cert
        // — important for the cache layer using it as a row key.
        let cert2 = public_view(&alice);
        assert_eq!(fp, cert2.fingerprint());
    }

    #[test]
    fn certificate_email_reads_subject_alt_name() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let cert = public_view(&alice);
        assert_eq!(cert.email().as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn certificate_subject_dn_contains_common_name_and_organisation() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let cert = public_view(&alice);
        let dn = cert.subject_dn();
        // We don't pin the exact ordering — OpenSSL's X509Name iteration
        // order follows the DER encoding which depends on the builder's
        // append order — but both attributes have to be there.
        assert!(dn.contains("CN=Alice Example"), "got: {dn:?}");
        assert!(dn.contains("O=Test"), "got: {dn:?}");
    }

    #[test]
    fn pem_round_trip_through_parse() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let pem = alice.leaf.to_pem().expect("test: serialize PEM");

        let parsed = parse_pem_cert(&pem).expect("parse PEM");
        // Same DER bytes → same fingerprint.
        assert_eq!(parsed.fingerprint(), alice.fingerprint());
    }

    #[test]
    fn pkcs12_round_trip_with_correct_passphrase() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        // Build a `.p12` envelope with a known passphrase, then parse
        // it back through our public API.  Exercises both the OpenSSL
        // PKCS#12 builder we'll lean on in tests and the
        // `parse_pkcs12` accessor's success path.
        let p12 = Pkcs12::builder()
            .name("Alice")
            .pkey(&alice.private_key)
            .cert(&alice.leaf)
            .build2("hunter2")
            .expect("test: build PKCS#12");
        let der = p12.to_der().expect("test: serialize PKCS#12");

        let parsed = parse_pkcs12(&der, "hunter2").expect("parse PKCS#12");
        assert_eq!(parsed.fingerprint(), alice.fingerprint());
    }

    #[test]
    fn pkcs12_wrong_passphrase_is_a_clear_error() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let p12 = Pkcs12::builder()
            .name("Alice")
            .pkey(&alice.private_key)
            .cert(&alice.leaf)
            .build2("hunter2")
            .expect("test: build PKCS#12");
        let der = p12.to_der().expect("test: serialize PKCS#12");

        let err = parse_pkcs12(&der, "wrong").expect_err("must reject wrong passphrase");
        assert!(
            matches!(err, UnkaiError::Crypto(ref m) if m.contains("Wrong PKCS#12 passphrase")),
            "got: {err:?}"
        );
    }
}

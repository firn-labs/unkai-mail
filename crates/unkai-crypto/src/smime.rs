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
//! - **MIME canonicalisation.**  The receive path hands [`smime_verify`]
//!   the exact on-the-wire bytes of the signed part and we verify them
//!   with the `BINARY` flag (no OpenSSL re-canonicalisation), so this
//!   module never reasons about CRLF normalisation — that's the protocol
//!   crates' concern on both the send and receive sides.
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

use openssl::asn1::Asn1Time;
use openssl::cms::{CMSOptions, CmsContentInfo};
use openssl::hash::MessageDigest;
use openssl::pkcs12::Pkcs12;
use openssl::pkey::{PKey, Private};
use openssl::stack::{Stack, StackRef};
use openssl::symm::Cipher;
use openssl::x509::store::{X509Store, X509StoreBuilder, X509StoreRef};
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

    /// `true` when the cert is outside its validity window — expired
    /// (`notAfter` in the past) *or* not yet valid (`notBefore` in the
    /// future).  Used by [`smime_verify`]'s trust model to downgrade an
    /// otherwise-sound signature to [`SignatureStatus::ValidExpiredCert`]
    /// (amber) — anchor 1 keeps the OpenSSL `X509` behind this façade so
    /// the protocol crates never reach for `not_after()` themselves.
    ///
    /// A clock we can't read (OpenSSL failing to produce "now", which
    /// should never happen) conservatively reports *not* expired — we'd
    /// rather not flag a good signature on a transient error.
    pub fn is_expired(&self) -> bool {
        let now = match Asn1Time::days_from_now(0) {
            Ok(t) => t,
            Err(_) => return false,
        };
        let expired = matches!(
            self.inner.not_after().compare(&now),
            Ok(std::cmp::Ordering::Less)
        );
        let not_yet_valid = matches!(
            self.inner.not_before().compare(&now),
            Ok(std::cmp::Ordering::Greater)
        );
        expired || not_yet_valid
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

/// Build an OpenSSL [`X509Store`] from the bundled Mozilla root CAs
/// (`webpki-root-certs`).  This is the trust anchor set [`smime_verify`]
/// checks a signing certificate's chain against.
///
/// We use the *bundled* Mozilla list rather than the OS trust store for
/// the same reason the TLS layer does (see the workspace `Cargo.toml`
/// note on `webpki-roots`): behaviour is then identical across
/// Win/Mac/Linux, and the vendored OpenSSL has no system trust directory
/// on Windows to fall back on anyway.
///
/// A root that fails to parse is skipped rather than aborting the whole
/// store — a single malformed entry shouldn't blind us to every other
/// CA.  Building costs ~150 DER parses; callers should build once per
/// receive batch rather than per message where practical.
pub fn build_mozilla_trust_store() -> Result<X509Store, UnkaiError> {
    let mut builder = X509StoreBuilder::new()
        .map_err(|e| UnkaiError::Crypto(format!("Failed to create X509 trust store: {e}")))?;
    for der in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
        if let Ok(cert) = X509::from_der(der.as_ref()) {
            // Ignore a per-cert add failure for the same reason we skip a
            // parse failure: one bad root shouldn't sink the store.
            let _ = builder.add_cert(cert);
        }
    }
    Ok(builder.build())
}

/// Outcome of verifying an S/MIME detached signature, carrying both the
/// trust-graded [`SignatureStatus`] and — when we could attribute the
/// signature to a certificate we already hold — that cert's fingerprint
/// for display.
///
/// `signer_fingerprint` is `Some` only for the TOFU path (the signature
/// matched a cert in `candidate_certs`); for a chain-only-trusted
/// signature it is `None`, because OpenSSL's Rust binding gives us no
/// way to read the signer certificate embedded in the `SignedData` back
/// out to fingerprint it.
#[derive(Debug, Clone)]
pub struct SmimeVerifyOutcome {
    pub status: SignatureStatus,
    pub signer_fingerprint: Option<String>,
}

/// Verify a CMS `SignedData` detached signature over `payload` and grade
/// the result against our trust model.  `candidate_certs` are the certs
/// we already hold for the claimed sender (their `smime_certs` rows);
/// `trust_store` is the bundled CA set from [`build_mozilla_trust_store`].
///
/// Returns a [`SmimeVerifyOutcome`] whose [`SignatureStatus`] drives the
/// UI's tri-tone chip:
/// - [`SignatureStatus::Valid`] (green) — math is sound *and* the signer
///   is trusted: either the signature matches a cert we already hold
///   (TOFU, with the fingerprint attributed) or the embedded signing
///   cert chains to a bundled CA root.
/// - [`SignatureStatus::ValidExpiredCert`] (amber) — math is sound but a
///   matching cert we hold is outside its validity window.
/// - [`SignatureStatus::ValidUntrustedIssuer`] (amber) — math is sound
///   but the signer is neither held nor chains to a trusted CA
///   (self-signed / unknown issuer).
/// - [`SignatureStatus::Invalid`] (red) — the signature does not verify.
/// - [`SignatureStatus::UnknownSigner`] (amber) — the message carried no
///   usable signer certificate to check against (conservative fallback,
///   mirroring [`crate::ops::verify_detached`]'s behaviour for OpenPGP).
///
/// ## Why three OpenSSL passes
///
/// openssl 0.10's `CmsContentInfo` exposes no accessor for the signer
/// certificate embedded in the `SignedData`, so we can't read it out to
/// inspect issuer / expiry directly.  Instead we determine trust by
/// *which combination of inputs makes `verify` succeed*:
/// 1. **TOFU** — per held cert, `NOINTERN` so OpenSSL only considers
///    that cert as the signer; success ⇒ the signature was made by a
///    cert we already trust (and we know its fingerprint + expiry).
/// 2. **CA chain** — drop `NOINTERN` so OpenSSL uses the embedded cert,
///    and hand it `trust_store`; success ⇒ chains to a public root.
/// 3. **Math only** — `NO_SIGNER_CERT_VERIFY`, no store; success ⇒ the
///    signature is internally sound but untrusted (amber); failure ⇒
///    tampered/`Invalid` (or no signer cert ⇒ `UnknownSigner`).
///
/// All passes use `DETACHED | BINARY`: the caller hands us the exact
/// on-the-wire signed bytes (see the receive path's signed-part
/// extraction), and `BINARY` tells OpenSSL not to re-canonicalise them —
/// we verify precisely what the signer hashed.
pub fn smime_verify(
    payload: &[u8],
    signature: &[u8],
    candidate_certs: &[&Certificate],
    trust_store: &X509Store,
) -> Result<SmimeVerifyOutcome, UnkaiError> {
    // Validate the signature DER up front so a genuinely malformed blob
    // surfaces as a protocol error rather than masquerading as a verify
    // failure (which we'd grade as Invalid).
    CmsContentInfo::from_der(signature)
        .map_err(|e| UnkaiError::Crypto(format!("Failed to parse CMS signature: {e}")))?;

    let detached = CMSOptions::DETACHED | CMSOptions::BINARY;

    // ── Pass 1: TOFU, per held cert ──────────────────────────────────
    // NOINTERN forces OpenSSL to take the signer only from the single
    // cert we provide, so a match attributes the signature to *that*
    // cert (giving us its fingerprint) and lets us check its expiry.
    let tofu_flags = detached | CMSOptions::NOINTERN | CMSOptions::NO_SIGNER_CERT_VERIFY;
    for cert in candidate_certs {
        let mut single: Stack<X509> = Stack::new()
            .map_err(|e| UnkaiError::Crypto(format!("Failed to create cert stack: {e}")))?;
        single
            .push(cert.inner.clone())
            .map_err(|e| UnkaiError::Crypto(format!("Failed to push candidate cert: {e}")))?;
        if cms_verify_ok(signature, payload, Some(&single), None, tofu_flags)? {
            let status = if cert.is_expired() {
                SignatureStatus::ValidExpiredCert
            } else {
                SignatureStatus::Valid
            };
            return Ok(SmimeVerifyOutcome {
                status,
                signer_fingerprint: Some(cert.fingerprint()),
            });
        }
    }

    // ── Pass 2: CA chain against the bundled Mozilla roots ───────────
    // No NOINTERN ⇒ OpenSSL uses the cert embedded in the SignedData to
    // find the signer, and validates its chain against `trust_store`.
    if cms_verify_ok(signature, payload, None, Some(trust_store), detached)? {
        return Ok(SmimeVerifyOutcome {
            status: SignatureStatus::Valid,
            signer_fingerprint: None,
        });
    }

    // ── Pass 3: math only — sound but untrusted, vs tampered ─────────
    let math_flags = detached | CMSOptions::NO_SIGNER_CERT_VERIFY;
    match cms_verify_result(signature, payload, None, None, math_flags)? {
        Ok(()) => {
            // Internally consistent but neither held nor chain-trusted.
            // If any cert we hold for this sender is expired, that's the
            // most useful explanation; otherwise it's an untrusted issuer.
            let status = if candidate_certs.iter().any(|c| c.is_expired()) {
                SignatureStatus::ValidExpiredCert
            } else {
                SignatureStatus::ValidUntrustedIssuer
            };
            Ok(SmimeVerifyOutcome {
                status,
                signer_fingerprint: None,
            })
        }
        Err(msg) => {
            // OpenSSL returns an `Err` for both "data tampered" and "no
            // signer certificate present".  A conformant detached S/MIME
            // signature always embeds its signer leaf, so a failure here
            // is overwhelmingly tampering ⇒ Invalid.  Only when the error
            // *specifically* blames a missing signer cert do we fall back
            // to the conservative UnknownSigner.  We match precise phrases
            // ("no signer", "unable to find …", "signer certificate not
            // found") rather than a bare "signer", because a genuine
            // digest mismatch surfaces as "signerInfo … verification
            // failure" — which contains "signer" but is emphatically
            // *tampering*, not a missing cert.
            let lower = msg.to_lowercase();
            let missing_signer = lower.contains("no signer")
                || lower.contains("signer certificate not found")
                || lower.contains("unable to find")
                || lower.contains("no matching signer");
            let status = if missing_signer {
                SignatureStatus::UnknownSigner
            } else {
                SignatureStatus::Invalid
            };
            Ok(SmimeVerifyOutcome {
                status,
                signer_fingerprint: None,
            })
        }
    }
}

/// Run one CMS verify attempt, re-parsing the signature DER fresh so
/// repeated passes don't share mutable `CmsContentInfo` state.  Returns
/// `Ok(true)` on a verified signature, `Ok(false)` on a verify failure,
/// and `Err` only if the DER won't parse (already ruled out by the
/// caller, but handled rather than panicking).
fn cms_verify_ok(
    signature: &[u8],
    payload: &[u8],
    certs: Option<&StackRef<X509>>,
    store: Option<&X509StoreRef>,
    flags: CMSOptions,
) -> Result<bool, UnkaiError> {
    Ok(cms_verify_result(signature, payload, certs, store, flags)?.is_ok())
}

/// Like [`cms_verify_ok`] but preserves the OpenSSL error message on a
/// verify failure so the caller can distinguish "tampered" from "no
/// signer certificate".
fn cms_verify_result(
    signature: &[u8],
    payload: &[u8],
    certs: Option<&StackRef<X509>>,
    store: Option<&X509StoreRef>,
    flags: CMSOptions,
) -> Result<Result<(), String>, UnkaiError> {
    let mut cms = CmsContentInfo::from_der(signature)
        .map_err(|e| UnkaiError::Crypto(format!("Failed to parse CMS signature: {e}")))?;
    Ok(cms
        .verify(certs, store, Some(payload), None, flags)
        .map_err(|e| e.to_string()))
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
        let value = entry.data().to_string().unwrap_or_else(|_| "?".to_string());
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
    use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
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

    /// Build a self-signed test CA root: an RSA-2048 cert with
    /// `basicConstraints: CA:TRUE` so OpenSSL will accept it as a chain
    /// anchor when we drop it into an `X509Store`.  Exercises the real
    /// chain-validation path in [`smime_verify`]'s Pass 2.
    fn make_test_ca(common_name: &str) -> CertificateWithKey {
        let rsa = Rsa::generate(2048).expect("test: generate CA RSA-2048");
        let pkey = PKey::from_rsa(rsa).expect("test: wrap CA RSA as PKey");

        let mut name_builder = X509NameBuilder::new().expect("test: CA name builder");
        name_builder
            .append_entry_by_text("CN", common_name)
            .expect("test: set CA CN");
        let name = name_builder.build();

        let mut cert_builder = X509Builder::new().expect("test: CA cert builder");
        cert_builder.set_version(2).expect("test: CA v3");
        let serial = BigNum::from_u32(1)
            .expect("test: CA serial bn")
            .to_asn1_integer()
            .expect("test: CA serial asn1");
        cert_builder
            .set_serial_number(&serial)
            .expect("test: set CA serial");
        cert_builder
            .set_subject_name(&name)
            .expect("test: CA subject");
        cert_builder
            .set_issuer_name(&name)
            .expect("test: CA issuer (self-signed)");
        cert_builder.set_pubkey(&pkey).expect("test: set CA pubkey");
        cert_builder
            .set_not_before(&Asn1Time::days_from_now(0).expect("test: CA not_before"))
            .expect("test: set CA not_before");
        cert_builder
            .set_not_after(&Asn1Time::days_from_now(3650).expect("test: CA not_after"))
            .expect("test: set CA not_after");
        // CA:TRUE — without basicConstraints OpenSSL won't treat this as a
        // CA and chain building against it fails.
        cert_builder
            .append_extension(
                BasicConstraints::new()
                    .critical()
                    .ca()
                    .build()
                    .expect("test: build CA basicConstraints"),
            )
            .expect("test: append CA basicConstraints");
        cert_builder
            .sign(&pkey, MessageDigest::sha256())
            .expect("test: self-sign CA");
        let cert = cert_builder.build();

        CertificateWithKey {
            leaf: cert,
            private_key: pkey,
            chain: vec![],
        }
    }

    /// Issue an end-entity leaf signed by `ca`, valid for `days_valid`
    /// days from now (pass a negative number for an already-expired
    /// cert).  The returned identity carries the CA in its `chain`, so
    /// [`smime_sign`] embeds it in the `SignedData` and a verifier can
    /// build the path leaf → CA.
    fn issue_leaf(
        ca: &CertificateWithKey,
        common_name: &str,
        email: &str,
        days_valid: i64,
    ) -> CertificateWithKey {
        let rsa = Rsa::generate(2048).expect("test: generate leaf RSA-2048");
        let pkey = PKey::from_rsa(rsa).expect("test: wrap leaf RSA as PKey");

        let mut name_builder = X509NameBuilder::new().expect("test: leaf name builder");
        name_builder
            .append_entry_by_text("CN", common_name)
            .expect("test: set leaf CN");
        let name = name_builder.build();

        let mut cert_builder = X509Builder::new().expect("test: leaf cert builder");
        cert_builder.set_version(2).expect("test: leaf v3");
        let serial = BigNum::from_u32(2)
            .expect("test: leaf serial bn")
            .to_asn1_integer()
            .expect("test: leaf serial asn1");
        cert_builder
            .set_serial_number(&serial)
            .expect("test: set leaf serial");
        cert_builder
            .set_subject_name(&name)
            .expect("test: leaf subject");
        // Issuer is the CA — this is what links the chain.
        cert_builder
            .set_issuer_name(ca.leaf.subject_name())
            .expect("test: leaf issuer = CA subject");
        cert_builder
            .set_pubkey(&pkey)
            .expect("test: set leaf pubkey");
        // `notBefore` a day in the past so a freshly-issued leaf isn't
        // tripped up by clock skew; `notAfter` per `days_valid`.
        cert_builder
            .set_not_before(&Asn1Time::days_from_now(0).expect("test: leaf not_before"))
            .expect("test: set leaf not_before");
        let not_after = if days_valid >= 0 {
            Asn1Time::days_from_now(days_valid as u32).expect("test: leaf not_after")
        } else {
            // Already expired: notAfter in the past.
            Asn1Time::from_unix(
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("test: now")
                    .as_secs() as i64)
                    + days_valid * 86_400,
            )
            .expect("test: expired not_after")
        };
        cert_builder
            .set_not_after(&not_after)
            .expect("test: set leaf not_after");
        let san = SubjectAlternativeName::new()
            .email(email)
            .build(&cert_builder.x509v3_context(Some(&ca.leaf), None))
            .expect("test: build leaf SAN");
        cert_builder
            .append_extension(san)
            .expect("test: append leaf SAN");
        // Signed by the CA's private key — not self-signed.
        cert_builder
            .sign(&ca.private_key, MessageDigest::sha256())
            .expect("test: CA-sign leaf");
        let cert = cert_builder.build();

        CertificateWithKey {
            leaf: cert,
            private_key: pkey,
            chain: vec![ca.leaf.clone()],
        }
    }

    /// An `X509Store` trusting exactly the given CA — stands in for the
    /// bundled Mozilla roots in chain-validation tests.
    fn trust_store_with(ca: &CertificateWithKey) -> X509Store {
        let mut b = X509StoreBuilder::new().expect("test: store builder");
        b.add_cert(ca.leaf.clone()).expect("test: add CA to store");
        b.build()
    }

    /// An empty `X509Store` — nothing is chain-trusted.
    fn empty_trust_store() -> X509Store {
        X509StoreBuilder::new()
            .expect("test: empty store builder")
            .build()
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
    fn verify_tofu_fingerprint_is_valid_green() {
        // A self-signed cert we already hold (passed as a candidate)
        // verifies its own signature ⇒ trusted by TOFU, fingerprint
        // attributed.
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let alice_cert = public_view(&alice);
        let payload = b"hello, world";

        let sig = smime_sign(payload, &alice).expect("sign");
        let outcome =
            smime_verify(payload, &sig, &[&alice_cert], &empty_trust_store()).expect("verify");

        assert_eq!(outcome.status, SignatureStatus::Valid);
        assert_eq!(
            outcome.signer_fingerprint.as_deref(),
            Some(alice_cert.fingerprint().as_str()),
            "TOFU match should attribute the signer's fingerprint"
        );
    }

    #[test]
    fn verify_chain_trusted_is_valid_green() {
        // A leaf issued by a CA we trust (in the store) ⇒ chain-trusted.
        // The signer cert is not in our candidate list, so attribution is
        // None (we can't read the embedded cert back out of the CMS).
        let ca = make_test_ca("Test Root CA");
        let alice = issue_leaf(&ca, "Alice Example", "alice@example.com", 365);
        let payload = b"chain trusted message";

        let sig = smime_sign(payload, &alice).expect("sign");
        let outcome = smime_verify(payload, &sig, &[], &trust_store_with(&ca)).expect("verify");

        assert_eq!(outcome.status, SignatureStatus::Valid);
        assert_eq!(outcome.signer_fingerprint, None);
    }

    #[test]
    fn verify_self_signed_unknown_issuer_is_amber() {
        // Self-signed, not held, empty store ⇒ math is sound but the
        // issuer is untrusted.
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let payload = b"unattested but well-formed";

        let sig = smime_sign(payload, &alice).expect("sign");
        let outcome = smime_verify(payload, &sig, &[], &empty_trust_store()).expect("verify");

        assert_eq!(outcome.status, SignatureStatus::ValidUntrustedIssuer);
        assert_eq!(outcome.signer_fingerprint, None);
    }

    #[test]
    fn verify_expired_signing_cert_is_amber() {
        // A cert we hold but which has since expired ⇒ the signature is
        // internally sound but the cert can no longer be relied on.
        let ca = make_test_ca("Test Root CA");
        let alice = issue_leaf(&ca, "Alice Example", "alice@example.com", -10);
        let alice_cert = public_view(&alice);
        let payload = b"signed with an expired cert";

        let sig = smime_sign(payload, &alice).expect("sign");
        // Pass the expired leaf as a held candidate: the TOFU pass matches
        // it, and the expiry check downgrades green → amber.
        let outcome =
            smime_verify(payload, &sig, &[&alice_cert], &trust_store_with(&ca)).expect("verify");

        assert_eq!(outcome.status, SignatureStatus::ValidExpiredCert);
    }

    #[test]
    fn verify_tampered_payload_is_red() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let alice_cert = public_view(&alice);
        let payload = b"original payload";
        let tampered = b"TAMPERED payload";

        let sig = smime_sign(payload, &alice).expect("sign");
        // The signed bytes don't match the signature ⇒ the math fails on
        // every pass ⇒ Invalid (red).
        let outcome =
            smime_verify(tampered, &sig, &[&alice_cert], &empty_trust_store()).expect("verify");

        assert_eq!(outcome.status, SignatureStatus::Invalid);
    }

    #[test]
    fn verify_wrong_held_cert_falls_through_to_untrusted() {
        // We hold Bob's cert but Alice signed.  TOFU can't match (NOINTERN
        // against Bob fails), there's no CA chain, but the embedded Alice
        // cert lets the math-only pass succeed ⇒ amber untrusted-issuer.
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let bob = make_test_cert("Bob Example", "bob@example.com");
        let bob_cert = public_view(&bob);
        let payload = b"signed by alice";

        let sig = smime_sign(payload, &alice).expect("sign");
        let outcome =
            smime_verify(payload, &sig, &[&bob_cert], &empty_trust_store()).expect("verify");

        assert_eq!(outcome.status, SignatureStatus::ValidUntrustedIssuer);
        assert_eq!(outcome.signer_fingerprint, None);
    }

    #[test]
    fn mozilla_trust_store_builds_non_empty() {
        // Smoke: the bundled root set parses into a store without error.
        // (We can't introspect the count through the OpenSSL binding, so
        // this just proves the builder path doesn't choke on the bundle.)
        build_mozilla_trust_store().expect("Mozilla trust store should build");
    }

    #[test]
    fn certificate_is_expired_accessor() {
        let ca = make_test_ca("Test Root CA");
        let fresh = issue_leaf(&ca, "Fresh", "fresh@example.com", 365);
        let stale = issue_leaf(&ca, "Stale", "stale@example.com", -5);
        assert!(!public_view(&fresh).is_expired());
        assert!(public_view(&stale).is_expired());
    }

    /// Regression guard (#370): the manual `Debug` impl on
    /// `CertificateWithKey` must never render the private key.  The leaf
    /// fingerprint (public) is fine to show; the key material must be
    /// redacted so it can't leak into a `tracing::debug!("{:?}")` call.
    #[test]
    fn certificate_with_key_debug_redacts_private_key() {
        let alice = make_test_cert("Alice Example", "alice@example.com");
        let rendered = format!("{alice:?}");
        assert!(
            rendered.contains("<redacted>"),
            "expected redaction marker in Debug output: {rendered}"
        );
        // The private key's PEM (or any PKCS#8 wrapper) must never appear.
        assert!(
            !rendered.contains("PRIVATE KEY"),
            "private key material leaked into Debug output: {rendered}"
        );
        // The public leaf fingerprint is fine — and proves we still
        // render something useful for debugging.
        assert!(
            rendered.contains(&alice.fingerprint()),
            "expected leaf fingerprint in Debug output: {rendered}"
        );
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

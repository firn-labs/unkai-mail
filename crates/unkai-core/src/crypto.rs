//! Cross-crate contract for end-to-end mail encryption (#57).
//!
//! `unkai-imap` and (in a follow-up) `unkai-jmap` need to decrypt
//! inbound mail before parsing it, but neither crate should pull in
//! the full OpenPGP dependency tree — they're protocol crates, and
//! adding `rpgp` and its ~40 transitive crypto crates to every IMAP
//! consumer would bloat downstream builds for no benefit.
//!
//! This module is the bridge: protocol crates take an `Option<&dyn CryptoBridge>`
//! at their parse / fetch entry points and call into it when they
//! detect a PGP/MIME envelope.  The concrete implementation lives at
//! the Tauri-command boundary, where it composes `unkai-crypto`
//! primitives with cache / keychain lookups for the active account.
//!
//! Passing `None` (or simply calling the un-suffixed entry points)
//! preserves the historical plaintext path — backwards-compatible
//! by construction.

use serde::{Deserialize, Serialize};

/// Outcome of decrypting an RFC-3156 `multipart/encrypted` payload,
/// optionally verifying an inner signature in the same pass.
///
/// `plaintext` is the recovered inner MIME message (headers + body)
/// — i.e. what the IMAP receive path will re-parse via `mail-parser`
/// to populate the rest of the `Email` struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecryptedPayload {
    /// Inner MIME bytes recovered from the OpenPGP envelope.
    pub plaintext: Vec<u8>,
    /// Kebab-case `unkai_crypto::SignatureStatus` when the encrypted
    /// payload also carried an OpenPGP one-pass signature.  `None`
    /// for encrypt-only messages.
    pub signature_status: Option<String>,
    /// Hex fingerprint of the verified signer when we matched the
    /// inner signature against a trusted public key.  `None` for
    /// encrypt-only messages or signatures from unknown senders.
    pub signer_fingerprint: Option<String>,
}

/// Outcome of verifying an RFC-3156 `multipart/signed` detached
/// signature against a trusted set of public keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyOutcome {
    /// Kebab-case `unkai_crypto::SignatureStatus`.  OpenPGP emits
    /// `"valid" | "invalid" | "unknown-signer"`; S/MIME can additionally
    /// emit the amber trust-nuance values `"valid-untrusted-issuer"` and
    /// `"valid-expired-cert"`.
    pub status: String,
    /// Identifier of the signer when we could attribute the signature:
    /// the key's hex fingerprint (OpenPGP) or the matched certificate's
    /// colon-hex SHA-256 fingerprint (S/MIME TOFU).  `None` when the
    /// signer couldn't be attributed (e.g. an S/MIME signature trusted
    /// only via its CA chain, whose embedded cert we can't read back).
    pub signer_fingerprint: Option<String>,
}

/// Output of encrypting an outbound mail body for one or more
/// recipients (#57).  The protocol crate that called the bridge wraps
/// this in an RFC-3156 `multipart/encrypted` envelope and hands the
/// result to the SMTP transport's `send_raw`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedOutput {
    /// `-----BEGIN PGP MESSAGE-----` armored ciphertext that goes in
    /// the `application/octet-stream` body part of the outer
    /// `multipart/encrypted` wrapper.  The wrapper itself is the
    /// SMTP layer's responsibility — the bridge only produces the
    /// payload that lives inside it.
    pub ciphertext_armor: Vec<u8>,
}

/// Contract the protocol crates use to delegate OpenPGP work.
///
/// Implemented at the Tauri-command boundary where the active
/// account's private key, passphrase, and the recipient public-key
/// cache are all in scope.  The implementation should be cheap to
/// construct per-request — protocol crates don't reuse it across
/// fetches.
///
/// `Send + Sync` so the trait object can cross an async boundary
/// inside the IMAP / JMAP fetch tasks without extra `Arc` wrapping
/// at the call site.
pub trait CryptoBridge: Send + Sync {
    /// Decrypt an armored OpenPGP message.  `ciphertext_armor` is the
    /// raw `-----BEGIN PGP MESSAGE-----` ASCII extracted from the
    /// PGP/MIME `application/octet-stream` part by the protocol crate.
    ///
    /// Returns a [`DecryptedPayload`] with the plaintext MIME bytes
    /// plus optional signature metadata if the message carried an
    /// inner one-pass signature that the implementation chose to
    /// verify.  Errors propagate the underlying crypto failure
    /// (`UnkaiError::Crypto`) which the protocol crate surfaces
    /// upward unchanged.
    fn decrypt(&self, ciphertext_armor: &[u8]) -> Result<DecryptedPayload, crate::UnkaiError>;

    /// Decrypt a CMS `EnvelopedData` blob (S/MIME, #338).  `cms_der` is
    /// the raw DER bytes the IMAP receive path lifted out of the
    /// `application/pkcs7-mime; smime-type=enveloped-data` part —
    /// mail-parser has already undone the base64 Content-Transfer-Encoding,
    /// so this is the binary CMS message, not its base64 text.
    ///
    /// Returns the same [`DecryptedPayload`] shape as [`Self::decrypt`]
    /// deliberately: the recovered inner MIME bytes plus optional
    /// signature metadata.  The two stacks funnel through one struct so
    /// the receive path, the cache rows, and the IPC payload never have
    /// to know which wire format produced the plaintext (the protocol
    /// distinction lives in the detect/apply code, not the data model).
    /// `signature_status` is `None` today — the nested
    /// `SignedData`-inside-`EnvelopedData` form (RFC 8551 §3.6) is a
    /// later sub-chunk — so an S/MIME enveloped message stamps
    /// `protection = "encrypted"` for now.
    ///
    /// Errors propagate the underlying `UnkaiError::Crypto` (e.g. the
    /// "wasn't encrypted to your certificate" sentinel from
    /// `unkai_crypto::smime_decrypt`) so the protocol crate surfaces it
    /// unchanged.
    fn decrypt_smime(&self, cms_der: &[u8]) -> Result<DecryptedPayload, crate::UnkaiError>;

    /// Verify a detached OpenPGP signature over a signed MIME body.
    /// `signed_payload` is the canonical form of the body part as it
    /// appeared between the multipart boundaries; `signature_armor`
    /// is the `-----BEGIN PGP SIGNATURE-----` blob from the
    /// `application/pgp-signature` part.
    ///
    /// Always returns a [`VerifyOutcome`] — implementations should
    /// map cryptographic failures to `status = "invalid"` and a
    /// missing trusted key to `status = "unknown-signer"`.  An
    /// `Err` here means the implementation couldn't even attempt
    /// verification (e.g. malformed signature packets).
    fn verify(
        &self,
        signed_payload: &[u8],
        signature_armor: &[u8],
    ) -> Result<VerifyOutcome, crate::UnkaiError>;

    /// Verify a detached S/MIME (CMS) signature over a signed MIME body
    /// (#338).  `signed_payload` is the exact on-the-wire bytes of the
    /// signed part as they sat between the `multipart/signed` boundaries
    /// (the receive path slices them straight out of the raw message —
    /// no re-canonicalisation, so what we verify is what the sender
    /// hashed); `signature_der` is the binary CMS `SignedData` from the
    /// `application/pkcs7-signature` part (mail-parser has already undone
    /// the base64 Content-Transfer-Encoding).
    ///
    /// `sender_from` is the message's `From` value — the implementation
    /// uses it to pull the sender's cached X.509 certs as TOFU candidates
    /// and to attribute the signature.  The returned [`VerifyOutcome`]
    /// carries the trust-graded status (`"valid"` / `"valid-untrusted-issuer"`
    /// / `"valid-expired-cert"` / `"invalid"` / `"unknown-signer"`) that
    /// drives the tri-tone signature chip; an `Err` means verification
    /// couldn't be attempted at all (e.g. an unparseable signature).
    fn verify_smime(
        &self,
        signed_payload: &[u8],
        signature_der: &[u8],
        sender_from: &str,
    ) -> Result<VerifyOutcome, crate::UnkaiError>;

    /// Encrypt an outbound mail body to one or more recipients,
    /// optionally signing with the active account's private key.
    /// `inner_mime` is the RFC-822-formatted body the SMTP layer
    /// would otherwise send in plaintext; the bridge looks up each
    /// recipient's public key from its cache and returns the
    /// armored OpenPGP ciphertext that the SMTP layer wraps in a
    /// `multipart/encrypted` envelope.
    ///
    /// `sign = true` triggers `sign_and_encrypt` (one-pass signature
    /// inside the encryption envelope, RFC 4880 §11.3); `false`
    /// produces an encrypt-only payload.  Implementations are
    /// expected to surface a `UnkaiError::CryptoKeyNotFound` for
    /// the first recipient whose key isn't cached, so the Compose
    /// layer can prompt the user to paste one in.
    fn encrypt(
        &self,
        inner_mime: &[u8],
        recipient_emails: &[String],
        sign: bool,
    ) -> Result<EncryptedOutput, crate::UnkaiError>;

    /// Produce a detached OpenPGP signature for an RFC 3156 §5
    /// `multipart/signed` outbound envelope.  `signed_payload` is the
    /// already-canonicalised body MIME entity (CRLF line endings,
    /// trailing whitespace stripped) — the SMTP layer is responsible
    /// for canonicalisation because RFC 3156 mandates it run before the
    /// hash is computed, and only the SMTP layer knows the exact bytes
    /// that will sit between the multipart boundaries on the wire.
    ///
    /// Returns the armored `-----BEGIN PGP SIGNATURE-----` blob that
    /// the SMTP layer drops into the `application/pgp-signature` body
    /// part of the outer `multipart/signed` wrapper.
    fn sign(&self, signed_payload: &[u8]) -> Result<Vec<u8>, crate::UnkaiError>;

    /// Encrypt an outbound mail body to one or more recipients as a CMS
    /// `EnvelopedData` blob (S/MIME, #338).  `inner_mime` is the
    /// RFC-822-formatted body the SMTP layer would otherwise send in
    /// plaintext; the bridge looks up each recipient's X.509 certificate
    /// from its `smime_certs` cache and returns the raw DER bytes the SMTP
    /// layer base64-wraps into the `application/pkcs7-mime;
    /// smime-type=enveloped-data` body (RFC 8551 §3.2).
    ///
    /// No `sign` flag, unlike [`Self::encrypt`]: the nested
    /// sign-then-encrypt form (RFC 8551 §3.6 — a `SignedData` wrapped
    /// inside the `EnvelopedData`) is deferred to a later sub-chunk, so
    /// this is encrypt-only for now.  Implementations surface
    /// `UnkaiError::CryptoKeyNotFound` for the first recipient whose cert
    /// isn't cached, so the Compose layer can prompt the user to import
    /// one — same contract as [`Self::encrypt`].
    fn encrypt_smime(
        &self,
        inner_mime: &[u8],
        recipient_emails: &[String],
    ) -> Result<Vec<u8>, crate::UnkaiError>;

    /// Produce a CMS `SignedData` *detached signature* for an RFC 8551
    /// §3.4 `multipart/signed; protocol="application/pkcs7-signature"`
    /// outbound envelope (S/MIME, #338).  `signed_payload` is the
    /// already-canonicalised body MIME entity — the SMTP layer owns
    /// canonicalisation for the same reason it does on the OpenPGP
    /// sign-only path (see [`Self::sign`]), and `unkai_crypto::smime_sign`
    /// signs with the `BINARY` flag so OpenSSL doesn't re-canonicalise.
    ///
    /// Returns the raw DER bytes the SMTP layer base64-wraps into the
    /// `application/pkcs7-signature` body part of the outer
    /// `multipart/signed` wrapper.  The signer's leaf cert (and any
    /// intermediates) ride along inside the `SignedData` so a recipient
    /// can identify us without an out-of-band cert fetch.
    fn sign_smime(&self, signed_payload: &[u8]) -> Result<Vec<u8>, crate::UnkaiError>;
}

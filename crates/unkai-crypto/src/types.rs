//! Shared status enums for cryptographic operations.
//!
//! These live in their own module so the cache layer (`unkai-store`) and
//! the Tauri IPC boundary (`src-tauri`) can depend on them *without*
//! pulling the OpenPGP stack — the rest of `unkai-crypto` only matters
//! at the actual encrypt / decrypt call sites.

use serde::{Deserialize, Serialize};

/// Cryptographic protection detected on (or being applied to) a message.
///
/// Modelled as a flat enum rather than a bitset because RFC 3156 only
/// defines four meaningful combinations and a flat enum round-trips
/// cleanly through `serde` for the cache + the Tauri IPC boundary.
///
/// String forms (used in JSON and SQL) are kebab-case to match the rest
/// of the codebase: `"none" | "signed" | "encrypted" | "signed-and-encrypted"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protection {
    /// Plain message — no PGP/MIME envelope, no S/MIME envelope.
    None,
    /// `multipart/signed` (PGP/MIME) or `multipart/signed; protocol=pkcs7-signature` (S/MIME).
    Signed,
    /// `multipart/encrypted` (PGP/MIME) or `application/pkcs7-mime; smime-type=enveloped-data` (S/MIME).
    Encrypted,
    /// Encrypted *and* the inner plaintext is itself a signed message.
    /// The common shape for mail you both want to keep private and want
    /// the recipient to be able to attribute to you.
    SignedAndEncrypted,
}

impl Protection {
    /// Did we see a signature anywhere in this message?  Convenience for
    /// the UI badge logic, which only cares whether to render a "signed"
    /// chip regardless of whether the body was also encrypted.
    pub fn is_signed(self) -> bool {
        matches!(self, Self::Signed | Self::SignedAndEncrypted)
    }

    /// Was the body encrypted on the wire?  Convenience for the UI badge
    /// that renders a "decrypted locally" chip.
    pub fn is_encrypted(self) -> bool {
        matches!(self, Self::Encrypted | Self::SignedAndEncrypted)
    }
}

/// Outcome of verifying a signature on an inbound message.
///
/// Distinct from `Protection` because a message can carry a signature
/// (so `Protection::Signed`) and yet that signature can be cryptographically
/// invalid, from a key we don't trust, or made with an expired certificate.
///
/// The UI renders a **tri-tone** chip from these variants:
/// - **green** — `Valid` (math sound *and* the signer is trusted)
/// - **amber** — `ValidUntrustedIssuer`, `ValidExpiredCert`, `UnknownSigner`
///   (math sound, but the trust story is incomplete)
/// - **red** — `Invalid` (math fails — tampered or wrong key)
///
/// The two `Valid…` amber variants exist so the chip tooltip can name
/// *why* a cryptographically sound signature still isn't fully trusted.
/// They are produced only by the S/MIME path (X.509 has a CA-chain and
/// certificate-expiry notion); the OpenPGP path has no CA concept and
/// only ever emits `Valid` / `Invalid` / `UnknownSigner`.  Keeping the
/// trust nuance *inside* this enum (rather than a parallel field) means
/// it rides the existing single kebab-case string end-to-end — no new
/// IPC field and no cache migration.
///
/// String forms: `"valid" | "invalid" | "unknown-signer" |
/// "valid-untrusted-issuer" | "valid-expired-cert"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureStatus {
    /// Signature math is sound *and* the signer is trusted — either the
    /// signing certificate chains to a bundled public CA root, or the
    /// signer's key/cert is one we already hold on file (TOFU).
    Valid,
    /// Signature payload didn't match — message tampered with, or the
    /// signer used a different key than the one we have on file.
    Invalid,
    /// Signature is well-formed but we don't have the signer's public
    /// key locally.  The user can still read the message; we just can't
    /// attribute it.  Common on a first contact before keys are exchanged.
    UnknownSigner,
    /// (S/MIME) Signature math is sound, but the signing certificate
    /// neither chains to a trusted CA root nor is one we already hold —
    /// e.g. a self-signed or unknown-issuer cert.  Amber: readable and
    /// internally consistent, but unattested.
    ValidUntrustedIssuer,
    /// (S/MIME) Signature math is sound, but the signing certificate is
    /// outside its validity window (expired or not-yet-valid).  Amber:
    /// the signature was likely fine when made, but the cert can no
    /// longer be relied on.
    ValidExpiredCert,
}

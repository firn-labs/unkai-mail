//! Opaque wrappers around `rpgp`'s transferable key types.
//!
//! We don't re-export `rpgp` types directly because:
//!
//! - It would force every downstream crate to depend on `rpgp` and inherit
//!   its ~40-crate dependency tree.
//! - It would couple our public API to `rpgp`'s 0.x versioning, which
//!   has shipped breaking changes every minor.  A future S/MIME branch
//!   (#338) might need to live behind the same façade.
//!
//! ## Passphrase handling
//!
//! [`PrivateKey`] carries the passphrase bytes alongside the key material.
//! We chose this shape (rather than "unlock once, hand back a decrypted
//! key handle") because `rpgp`'s `MessageBuilder` and `DetachedSignature`
//! APIs want a fresh [`rpgp::types::Password`] on each operation —
//! they re-derive the encryption key internally.  Carrying the password
//! in the wrapper keeps callers from threading it through every call site.
//!
//! The cleartext lives only in memory and only for the duration of the
//! wrapper.  Per the "re-prompt on every operation" decision in #57, the
//! wrapper is created immediately before a single send / decrypt call
//! and dropped when the call returns.

use rpgp::composed::{Deserializable, SignedPublicKey, SignedSecretKey};
use rpgp::types::{KeyDetails, Password};
use unkai_core::UnkaiError;
use zeroize::Zeroizing;

/// A recipient's (or our own) public key, parsed and self-signature-verified.
///
/// The wrapped `SignedPublicKey` is `rpgp`'s "Transferable Public Key" —
/// it carries one primary key, zero or more subkeys, and the self-signatures
/// that tie them together.  We don't currently expose subkey selection;
/// `rpgp` picks an appropriate encryption-capable subkey automatically.
#[derive(Debug, Clone)]
pub struct PublicKey {
    pub(crate) inner: SignedPublicKey,
}

impl PublicKey {
    /// Hex-encoded uppercase fingerprint of the primary key, matching the
    /// form GnuPG and most mail clients show in their UI.  Used as the
    /// cache key in our `pgp_public_keys` table and as the human-readable
    /// identifier shown under each contact / account.
    pub fn fingerprint(&self) -> String {
        format!("{:X}", self.inner.fingerprint())
    }
}

/// Our own private key, paired with the passphrase that unlocks it.
///
/// Only ever held in memory for the duration of one crypto operation —
/// see module docs for the lifetime story.
pub struct PrivateKey {
    pub(crate) inner: SignedSecretKey,
    /// Raw passphrase bytes.  We materialise a fresh `Password` per op
    /// via [`Self::password`] because `rpgp`'s `Password` enum doesn't
    /// implement `Clone` and several call sites consume it by value.
    ///
    /// Wrapped in [`Zeroizing`] so the cleartext is scrubbed from memory
    /// when this `PrivateKey` drops, rather than lingering in freed heap
    /// (and potentially swap) until the allocator happens to reuse it.
    pub(crate) password_bytes: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for PrivateKey {
    /// Manual `Debug` impl that elides the passphrase.  Auto-derive
    /// would risk leaking it into log output via `tracing::debug!`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateKey")
            .field("fingerprint", &format!("{:X}", self.inner.fingerprint()))
            .field("password", &"<redacted>")
            .finish()
    }
}

impl PrivateKey {
    /// Hex-encoded uppercase fingerprint of the primary key.  Pairs with
    /// stored public-key metadata in the UI.
    pub fn fingerprint(&self) -> String {
        format!("{:X}", self.inner.fingerprint())
    }

    /// Build a fresh `Password` that the `rpgp` API can consume by value.
    /// Cheap allocation; the cleartext lives only inside the returned
    /// `Password` and is zeroized on drop by the `zeroize` crate that
    /// `rpgp` already wraps it in.
    pub(crate) fn password(&self) -> Password {
        Password::from(self.password_bytes.as_slice())
    }
}

/// Parse an armored (ASCII `-----BEGIN PGP PUBLIC KEY BLOCK-----`) or
/// binary OpenPGP public-key blob into a [`PublicKey`].
///
/// Auto-detects armor by sniffing for the BEGIN line; this matches what
/// every other PGP client does and lets users paste a key from anywhere
/// without picking a format.
pub fn parse_public_key(bytes: &[u8]) -> Result<PublicKey, UnkaiError> {
    let parsed = if looks_armored(bytes) {
        SignedPublicKey::from_armor_single(std::io::Cursor::new(bytes))
            .map_err(|e| UnkaiError::Crypto(format!("Failed to parse armored public key: {e}")))?
            .0
    } else {
        SignedPublicKey::from_bytes(std::io::Cursor::new(bytes))
            .map_err(|e| UnkaiError::Crypto(format!("Failed to parse binary public key: {e}")))?
    };

    parsed
        .verify_bindings()
        .map_err(|e| UnkaiError::Crypto(format!("Public key self-signature invalid: {e}")))?;

    Ok(PublicKey { inner: parsed })
}

/// Parse an armored or binary OpenPGP secret-key blob with the given
/// passphrase.  The passphrase is *not* checked against the key here —
/// `rpgp` defers decryption of the secret material to use time.  Pass
/// `None` for an unprotected key (rare in practice; we still allow it).
pub fn parse_private_key(bytes: &[u8], passphrase: Option<&str>) -> Result<PrivateKey, UnkaiError> {
    let parsed = if looks_armored(bytes) {
        SignedSecretKey::from_armor_single(std::io::Cursor::new(bytes))
            .map_err(|e| UnkaiError::Crypto(format!("Failed to parse armored secret key: {e}")))?
            .0
    } else {
        SignedSecretKey::from_bytes(std::io::Cursor::new(bytes))
            .map_err(|e| UnkaiError::Crypto(format!("Failed to parse binary secret key: {e}")))?
    };

    parsed
        .verify_bindings()
        .map_err(|e| UnkaiError::Crypto(format!("Secret key self-signature invalid: {e}")))?;

    let password_bytes = Zeroizing::new(
        passphrase
            .map(|s| s.as_bytes().to_vec())
            .unwrap_or_default(),
    );

    Ok(PrivateKey {
        inner: parsed,
        password_bytes,
    })
}

/// Cheap heuristic — does the byte slice start with `-----BEGIN ` after
/// any leading whitespace?  Binary OpenPGP packets always start with a
/// packet tag byte that has its high bit set, never with `-`, so this is
/// unambiguous.
pub(crate) fn looks_armored(bytes: &[u8]) -> bool {
    let trimmed = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|i| &bytes[i..])
        .unwrap_or(bytes);
    trimmed.starts_with(b"-----BEGIN ")
}

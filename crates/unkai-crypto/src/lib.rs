//! Unkai Crypto — end-to-end email encryption primitives.
//!
//! This crate is the single home for sign / verify / encrypt / decrypt
//! over RFC-3156 PGP/MIME and RFC-8551 S/MIME.  Keeping it separate
//! from the protocol crates (`unkai-imap`, `unkai-smtp`, `unkai-jmap`)
//! is deliberate: those crates already pull in the TLS / networking
//! dependency tree, and we don't want OpenPGP / X.509 parsing
//! transitively dragged through every build that just wants to fetch
//! a mailbox.
//!
//! ## Module layout
//!
//! - [`types`] — shared status enums (`Protection`, `SignatureStatus`)
//!   used by the cache layer and the Tauri IPC boundary.  Variants are
//!   protocol-agnostic; both PGP and S/MIME funnel through the same
//!   `Protection::{Signed, Encrypted, SignedAndEncrypted}`.
//! - [`keys`] — opaque wrappers around `rpgp`'s `SignedPublicKey` /
//!   `SignedSecretKey` plus parsers from armored or binary bytes.
//! - [`ops`] — OpenPGP crypto primitives: `encrypt`, `decrypt_and_verify`,
//!   `sign_detached`, `verify_detached`, `sign_and_encrypt`.
//! - [`smime`] — S/MIME counterparts: `smime_sign`, `smime_verify`,
//!   `smime_encrypt`, `smime_decrypt`, plus X.509 `Certificate` /
//!   `CertificateWithKey` wrappers and `parse_pem_cert` /
//!   `parse_der_cert` / `parse_pkcs12` parsers.
//!
//! ## What this crate does *not* do
//!
//! No MIME envelope construction.  RFC 3156 (PGP/MIME) and RFC 8551
//! (S/MIME) both say "wrap the ciphertext in a particular MIME shell"
//! but building that MIME wrapper belongs with the SMTP send path
//! (which already owns MIME assembly via `lettre`).  This crate hands
//! back armored OpenPGP blobs or DER-encoded CMS bytes; the protocol
//! crates wrap them.
//!
//! See [issue #57](https://github.com/firn-labs/unkai-mail/issues/57)
//! for the OpenPGP half and
//! [issue #338](https://github.com/firn-labs/unkai-mail/issues/338)
//! for the S/MIME half.

pub mod keys;
pub mod ops;
pub mod smime;
pub mod types;

pub use keys::{PrivateKey, PublicKey, parse_private_key, parse_public_key};
pub use ops::{
    DecryptedMessage, decrypt_and_verify, encrypt, sign_and_encrypt, sign_detached, verify_detached,
};
pub use smime::{
    Certificate, CertificateWithKey, DecryptedSmimeMessage, SmimeVerifyOutcome,
    build_mozilla_trust_store, parse_der_cert, parse_pem_cert, parse_pkcs12, smime_decrypt,
    smime_encrypt, smime_sign, smime_verify,
};
pub use types::{Protection, SignatureStatus};

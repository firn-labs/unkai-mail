//! Unkai Crypto — end-to-end email encryption primitives.
//!
//! This crate is the single home for sign / verify / encrypt / decrypt
//! over RFC-3156 PGP/MIME (and, in a follow-up, RFC-8551 S/MIME).  Keeping
//! it separate from the protocol crates (`unkai-imap`, `unkai-smtp`,
//! `unkai-jmap`) is deliberate: those crates already pull in the TLS /
//! networking dependency tree, and we don't want OpenPGP / X.509 parsing
//! transitively dragged through every build that just wants to fetch
//! a mailbox.
//!
//! ## Module layout
//!
//! - [`types`] — shared status enums (`Protection`, `SignatureStatus`)
//!   used by the cache layer and the Tauri IPC boundary.
//! - [`keys`] — opaque wrappers around `rpgp`'s `SignedPublicKey` /
//!   `SignedSecretKey` plus parsers from armored or binary bytes.
//! - [`ops`] — the actual crypto primitives: `encrypt`, `decrypt_and_verify`,
//!   `sign_detached`, `verify_detached`, `sign_and_encrypt`.
//!
//! ## What this crate does *not* do
//!
//! No MIME envelope construction.  RFC 3156 says "wrap the ciphertext in
//! a `multipart/encrypted; protocol=application/pgp-encrypted`" but
//! building that MIME wrapper belongs with the SMTP send path (which
//! already owns MIME assembly via `lettre`).  This crate hands back
//! armored OpenPGP blobs; the protocol crates wrap them.
//!
//! See [issue #57](https://github.com/firn-labs/unkai-mail/issues/57).

pub mod keys;
pub mod ops;
pub mod types;

pub use keys::{PrivateKey, PublicKey, parse_private_key, parse_public_key};
pub use ops::{
    DecryptedMessage, decrypt_and_verify, encrypt, sign_and_encrypt, sign_detached, verify_detached,
};
pub use types::{Protection, SignatureStatus};

//! Unkai Core — shared types, traits, and domain models for the Unkai mail client.

pub mod crypto;
pub mod error;
pub mod mail_util;
pub mod models;
pub mod tls;
pub mod url;

pub use crypto::{CryptoBridge, DecryptedPayload, EncryptedOutput, VerifyOutcome};
pub use error::UnkaiError;

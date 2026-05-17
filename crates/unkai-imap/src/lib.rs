//! Unkai IMAP — handles mail retrieval over IMAP.
//!
//! This crate provides async IMAP connectivity for fetching,
//! syncing, and managing mailboxes.

mod client;
mod mutf7;

pub use client::{
    EnvelopeBatch, FlagSnapshot, ImapClient, parse_eml_bytes, probe_server_certificate,
};

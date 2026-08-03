//! Unkai IMAP — handles mail retrieval over IMAP.
//!
//! This crate provides async IMAP connectivity for fetching,
//! syncing, and managing mailboxes.

mod attachment_filename;
mod client;
mod mutf7;

pub use client::{
    EnvelopeBatch, FlagSnapshot, ImapClient, MdnReportData, extract_decrypted_attachment,
    parse_eml_bytes, parse_eml_bytes_with_crypto, parse_mdn_report, probe_server_certificate,
};

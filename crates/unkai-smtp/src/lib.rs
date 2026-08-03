//! Unkai SMTP — handles sending mail over SMTP.
//!
//! This crate provides async SMTP connectivity for composing
//! and sending email messages via [`SmtpClient`].

pub mod client;
pub use client::{MdnReply, SmtpClient, build_mdn_report_bytes, build_outgoing_message};

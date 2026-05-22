//! Unkai JMAP — modern mail access via the JMAP protocol (RFC 8620 / 8621).
//!
//! JMAP (JSON Meta Application Protocol) is a more efficient alternative
//! to IMAP for servers that support it. Instead of a line-based protocol
//! over a persistent TCP connection, JMAP uses standard HTTP POST with
//! JSON request/response bodies. This means:
//!
//! - **Fewer round-trips**: multiple operations can be batched into a
//!   single HTTP request.
//! - **Structured data**: responses are typed JSON, not ad-hoc text that
//!   needs fragile parsing.
//! - **Push support**: real-time notifications via Server-Sent Events.
//! - **Stateless**: no session to keep alive — each request is self-contained.
//!
//! # Architecture
//!
//! The entry point is [`JmapClient`]. It mirrors the interface of
//! `unkai_imap::ImapClient` so the Tauri command layer can switch between
//! the two based on the account's `use_jmap` flag.
//!
//! ```ignore
//! let client = JmapClient::connect("https://mail.example.com", "user", "pass").await?;
//! let folders = client.list_folders().await?;
//! let envelopes = client.fetch_envelopes("Inbox", 50).await?;
//! ```

mod client;
mod types;

pub use client::JmapClient;

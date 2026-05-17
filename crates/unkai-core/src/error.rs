//! Shared error types for Unkai.

use serde::Serialize;
use thiserror::Error;

/// The central error type used throughout all Unkai crates.
///
/// It derives `Serialize` so that Tauri can send errors across
/// the IPC boundary to the frontend as JSON.
#[derive(Debug, Error, Serialize)]
pub enum UnkaiError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    /// Fetch (IMAP/JMAP) returned no results for a UID we asked for —
    /// the message no longer exists on the server (deleted by another
    /// client, expunged by a server-side rule, or invalidated by a
    /// UIDVALIDITY reset).  Distinct from `Protocol` so the Tauri
    /// command layer can evict the dead envelope from the cache + the
    /// frontend can auto-advance to the next neighbour instead of
    /// surfacing a raw "No message with UID 3056" string.
    #[error("Message no longer exists on the server")]
    MessageGone,

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Nextcloud API error: {0}")]
    Nextcloud(String),

    /// CalDAV / WebDAV `If-Match` precondition failed — the
    /// resource on the server has a newer etag than the one we
    /// cached.  Distinct from `Nextcloud` so callers can detect
    /// it programmatically and retry transparently (sync to
    /// refresh the cached etag → re-attempt the PUT) instead of
    /// surfacing a "refresh and try again" toast to the user.
    #[error("Resource changed on the server since last sync: {0}")]
    EtagMismatch(String),

    /// CalDAV / WebDAV write refused with 403 Forbidden or 404
    /// Not Found (#236 follow-up).  Distinguished from
    /// `Nextcloud` so the calling Tauri command can react —
    /// flip the calendar's `read_only` flag in the local cache
    /// + emit `calendars-updated` so the EventEditor stops
    /// offering Save / Delete on this and any other event in
    /// that calendar.  Sabre/DAV (NC's CalDAV stack) commonly
    /// returns 404 instead of 403 for forbidden resources as a
    /// permission-masking pattern, so we treat both the same.
    #[error("CalDAV write forbidden: {0}")]
    CalDavWriteForbidden(String),

    #[error("{0}")]
    Other(String),
}

//! Nextcloud Notes integration â€” list / get / create / update / delete
//! markdown notes via the Notes app's REST API.
//!
//! # Endpoint shape
//!
//! Unlike Talk and Files, Notes lives outside OCS. Every call hits
//! `/index.php/apps/notes/api/v1/notes` with HTTP Basic auth and
//! plain JSON in / out â€” no `OCS-APIRequest` header, no two-level
//! `ocs.data` envelope, just the resource itself.
//!
//! Auth is the same app-password Basic auth Talk and Files use; the
//! caller pulls it from the keychain and hands it in.
//!
//! # MVP scope (issue #67)
//!
//! - [`list_notes`] â€” fetch every note the user has access to.
//! - [`get_note`] â€” fetch a single note by id (used after the list to
//!   pull the body, since list responses include it but a fresh fetch
//!   guarantees the latest etag for the upcoming PUT).
//! - [`create_note`] â€” POST a brand-new note.
//! - [`update_note`] â€” PUT title / content / category changes.
//! - [`delete_note`] â€” drop a note.
//!
//! Categories and the `favorite` flag round-trip cleanly but the UI
//! doesn't surface them in this slice â€” they're carried so a future
//! iteration can add filtering / pinning without a wire-format change.

use serde::{Deserialize, Serialize};

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;

use crate::client;

/// One note as the UI cares about it. Mirrors the Notes app's JSON
/// shape exactly so we can serde the wire format straight in / out
/// without an intermediate `Wire*` struct (the Talk / Shares modules
/// need the wire-vs-domain split because OCS wraps everything; Notes
/// doesn't, so the extra type would be pure ceremony).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    /// Server-assigned id. `0` on a fresh local draft before
    /// `create_note` returns â€” the server stamps the real id.
    pub id: u64,
    /// Optimistic-concurrency token. Sent back on `update_note` via
    /// the `If-Match` header; the server rejects the PUT with `412`
    /// if another client has saved in the meantime.
    #[serde(default)]
    pub etag: String,
    /// Unix epoch seconds of the last modification.
    #[serde(default)]
    pub modified: i64,
    /// Note title â€” derived by the server from the first line of
    /// `content` if the client doesn't set it explicitly. We mirror
    /// the server's behaviour by sending an empty title on create
    /// and letting it auto-fill, but `update_note` accepts a
    /// caller-supplied override.
    #[serde(default)]
    pub title: String,
    /// Folder-style grouping (e.g. `"Work/Meetings"`). Empty string
    /// = uncategorized. Round-trips cleanly even though the current
    /// UI ignores it.
    #[serde(default)]
    pub category: String,
    /// Markdown body. The Notes web UI renders this with its own
    /// markdown pipeline; Unkai shows it plain in the MVP and lets
    /// markdown round-trip through the editor.
    #[serde(default)]
    pub content: String,
    /// User-pinned flag. Round-trips but isn't surfaced in the MVP.
    #[serde(default)]
    pub favorite: bool,
}

/// Body of a `create_note` POST. Separate from `Note` so callers
/// don't have to invent an `id` / `etag` / `modified` for a note
/// that doesn't exist yet â€” the server fills those in on the
/// response.
#[derive(Debug, Clone, Serialize)]
pub struct NewNote<'a> {
    pub title: &'a str,
    pub content: &'a str,
    pub category: &'a str,
}

/// Body of an `update_note` PUT. Each field is optional â€” the
/// server only touches the ones we send, so a category-only edit
/// doesn't have to round-trip the whole note. The Notes app
/// accepts these field names verbatim under
/// `PUT /apps/notes/api/v1/notes/{id}`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct NoteUpdate<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,
}

/// Path prefix shared by every Notes endpoint. Pulled out as a const
/// so the URL shape lives in one place â€” if Notes ever ships a v2
/// (currently no signs of it) we change this constant.
const NOTES_BASE: &str = "/index.php/apps/notes/api/v1/notes";

/// List every note the current user has access to. Returns an empty
/// list (not an error) when the user has no notes yet â€” that's the
/// "first launch of NotesView" state the UI's empty placeholder
/// covers.
pub async fn list_notes(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<Note>, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}{NOTES_BASE}");

    tracing::debug!("GET {url}");
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("notes list request failed: {e}")))?;

    expect_success(&resp, "list notes")?;
    resp.json::<Vec<Note>>()
        .await
        .map_err(|e| UnkaiError::Protocol(format!("notes list parse failed: {e}")))
}

/// Fetch a single note by id. The list response already includes
/// `content`, so this is mostly used right before `update_note` to
/// pick up the freshest `etag` and avoid a 412 collision with a
/// concurrent edit from the web UI.
pub async fn get_note(
    server_url: &str,
    username: &str,
    app_password: &str,
    id: u64,
    trusted_certs: &[TrustedCert],
) -> Result<Note, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}{NOTES_BASE}/{id}");

    tracing::debug!("GET {url}");
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("notes get request failed: {e}")))?;

    expect_success(&resp, "get note")?;
    resp.json::<Note>()
        .await
        .map_err(|e| UnkaiError::Protocol(format!("notes get parse failed: {e}")))
}

/// Create a new note and return the server's freshly-stamped row.
/// The server derives `title` from the first line of `content` when
/// `title` is empty â€” pass `""` to get that auto-fill behaviour, or
/// a real title to pin one.
pub async fn create_note(
    server_url: &str,
    username: &str,
    app_password: &str,
    note: &NewNote<'_>,
    trusted_certs: &[TrustedCert],
) -> Result<Note, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}{NOTES_BASE}");

    tracing::debug!("POST {url}");
    let http = client::build(trusted_certs)?;
    let resp = http
        .post(&url)
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .json(note)
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("notes create request failed: {e}")))?;

    expect_success(&resp, "create note")?;
    resp.json::<Note>()
        .await
        .map_err(|e| UnkaiError::Protocol(format!("notes create parse failed: {e}")))
}

/// Apply a partial update to an existing note. `etag` is the value
/// the caller saw on its last `get_note` (or `list_notes`); the
/// server returns 412 Precondition Failed if another writer has
/// touched the note in the meantime â€” the caller should re-fetch
/// and retry / merge.
pub async fn update_note(
    server_url: &str,
    username: &str,
    app_password: &str,
    id: u64,
    etag: &str,
    update: &NoteUpdate<'_>,
    trusted_certs: &[TrustedCert],
) -> Result<Note, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}{NOTES_BASE}/{id}");

    tracing::debug!("PUT {url}");
    let http = client::build(trusted_certs)?;
    let mut req = http
        .put(&url)
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .json(update);
    if !etag.is_empty() {
        req = req.header("If-Match", format!("\"{etag}\""));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("notes update request failed: {e}")))?;

    expect_success(&resp, "update note")?;
    resp.json::<Note>()
        .await
        .map_err(|e| UnkaiError::Protocol(format!("notes update parse failed: {e}")))
}

/// Delete a note by id. Idempotent on the server â€” deleting a
/// missing note returns 404, which we propagate as `NotFound` so
/// the UI can stay quiet on a double-click of the trash button.
pub async fn delete_note(
    server_url: &str,
    username: &str,
    app_password: &str,
    id: u64,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}{NOTES_BASE}/{id}");

    tracing::debug!("DELETE {url}");
    let http = client::build(trusted_certs)?;
    let resp = http
        .delete(&url)
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("notes delete request failed: {e}")))?;

    expect_success(&resp, "delete note")?;
    Ok(())
}

/// Notes uses regular HTTP status codes (no OCS envelope), so a
/// non-2xx is a real error â€” surface a typed variant for the
/// common cases instead of always tossing the body into a generic
/// `Protocol` so the UI can react meaningfully (re-auth on 401,
/// "note not found" on 404, conflict-recovery on 412).
fn expect_success(resp: &reqwest::Response, op: &str) -> Result<(), UnkaiError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    Err(match status.as_u16() {
        401 => UnkaiError::Auth(format!("{op}: not authenticated")),
        403 => UnkaiError::Auth(format!("{op}: forbidden")),
        // No dedicated `NotFound` variant; `Nextcloud` is the
        // closest fit and surfaces the per-app context cleanly.
        404 => UnkaiError::Nextcloud(format!("{op}: note not found")),
        // Reuse the same etag-collision variant CalDAV uses so
        // future "refresh and retry" plumbing in the UI can match
        // on a single error type across protocols.
        412 => UnkaiError::EtagMismatch(format!("{op}: note was modified by another client")),
        _ => UnkaiError::Network(format!("{op}: HTTP {status}")),
    })
}

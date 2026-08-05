//! Nextcloud Notes.
//!
//! Mirrors `ui/src/lib/api/notes.ts`.

use unkai_core::UnkaiError;
use unkai_store::Cache;
use unkai_store::credentials;

use crate::support::load_nextcloud_account;

// ── Nextcloud Notes (issue #67) ────────────────────────────────
//
// Five thin commands wrapping `unkai_nextcloud::notes`. Same
// shape as the Talk block above: each call loads the chosen NC
// account + app password and forwards. The Notes API is plain
// REST under `/index.php/apps/notes/api/v1/notes`, so there's no
// envelope unpacking — the wire types come straight back.
//
// We deliberately don't cache notes locally: the Notes web UI is
// the canonical editor and we want NotesView to reflect what the
// user just typed there without a sync-roundtrip dance. Cost is
// one HTTP call per list-refresh, which is cheap.

/// Convert the wire-shape `unkai_nextcloud::Note` (which doesn't
/// know about accounts) into the canonical `unkai_core::models::Note`
/// we cache and ship to the UI.  Stamping the account id at the
/// boundary keeps the `Note` type a single source of truth across
/// the codebase.
pub fn nc_note_to_core(nc_id: &str, n: unkai_nextcloud::Note) -> unkai_core::models::Note {
    unkai_core::models::Note {
        id: n.id,
        nextcloud_account_id: nc_id.to_string(),
        etag: n.etag,
        modified: n.modified,
        title: n.title,
        category: n.category,
        content: n.content,
        favorite: n.favorite,
    }
}

/// Cache-first list (#138).  Returns whatever's on disk so the UI
/// paints instantly; the frontend kicks off a background sync via
/// `sync_nextcloud_notes` to refresh.  Mirrors how `get_contacts`
/// and the mail list work.
pub fn list_nextcloud_notes(
    nc_id: String,
    cache: &Cache,
) -> Result<Vec<unkai_core::models::Note>, UnkaiError> {
    cache.list_notes(&nc_id).map_err(Into::into)
}

/// Pull every note from the server, diff against the cache, and
/// persist the result transactionally.  Server-deleted notes
/// disappear from the cache as part of the same delta.  Returns
/// the fresh list so the caller can update its state without a
/// second round-trip through `list_nextcloud_notes`.
pub async fn sync_nextcloud_notes(
    nc_id: String,
    cache: &Cache,
) -> Result<Vec<unkai_core::models::Note>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let server = unkai_nextcloud::list_notes(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    let notes: Vec<unkai_core::models::Note> = server
        .into_iter()
        .map(|n| nc_note_to_core(&nc_id, n))
        .collect();
    cache.apply_notes_delta(&nc_id, &notes)?;
    Ok(notes)
}

/// Fetch a single note from the server (refreshing its etag) and
/// upsert it into the cache.  Used right before an edit lands so a
/// 412 doesn't fire because the user looked at a stale note ages
/// ago.
pub async fn get_nextcloud_note(
    nc_id: String,
    note_id: u64,
    cache: &Cache,
) -> Result<unkai_core::models::Note, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let server = unkai_nextcloud::get_note(
        &account.server_url,
        &account.username,
        &app_password,
        note_id,
        &account.trusted_certs,
    )
    .await?;
    let note = nc_note_to_core(&nc_id, server);
    cache.upsert_note(&note)?;
    Ok(note)
}

/// Create a new note. Title can be empty — the server derives it
/// from the first content line in that case, matching the behaviour
/// of the Notes web UI.  Cache-write-through: the server stamps
/// the id + etag, then we persist locally before returning.
pub async fn create_nextcloud_note(
    nc_id: String,
    title: String,
    content: String,
    category: String,
    cache: &Cache,
) -> Result<unkai_core::models::Note, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let server = unkai_nextcloud::create_note(
        &account.server_url,
        &account.username,
        &app_password,
        &unkai_nextcloud::NewNote {
            title: &title,
            content: &content,
            category: &category,
        },
        &account.trusted_certs,
    )
    .await?;
    let note = nc_note_to_core(&nc_id, server);
    cache.upsert_note(&note)?;
    Ok(note)
}

/// Apply a partial update. Each field is optional — the frontend
/// sends only the ones the user touched so a category-only edit
/// doesn't have to round-trip body bytes the user didn't change.
/// Cache-write-through: the server is authoritative on etag /
/// modified; we persist what it returns.
#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
pub async fn update_nextcloud_note(
    nc_id: String,
    note_id: u64,
    etag: String,
    title: Option<String>,
    content: Option<String>,
    category: Option<String>,
    favorite: Option<bool>,
    cache: &Cache,
) -> Result<unkai_core::models::Note, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let server = unkai_nextcloud::update_note(
        &account.server_url,
        &account.username,
        &app_password,
        note_id,
        &etag,
        &unkai_nextcloud::NoteUpdate {
            title: title.as_deref(),
            content: content.as_deref(),
            category: category.as_deref(),
            favorite,
        },
        &account.trusted_certs,
    )
    .await?;
    let note = nc_note_to_core(&nc_id, server);
    cache.upsert_note(&note)?;
    Ok(note)
}

/// Delete a note. Server first (so a 4xx surfaces before we touch
/// local state); cache delete only runs on success so a network
/// failure leaves the user's note intact locally.
pub async fn delete_nextcloud_note(
    nc_id: String,
    note_id: u64,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::delete_note(
        &account.server_url,
        &account.username,
        &app_password,
        note_id,
        &account.trusted_certs,
    )
    .await?;
    cache.delete_note(&nc_id, note_id)?;
    Ok(())
}

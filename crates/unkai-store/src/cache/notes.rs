//! Nextcloud Notes cache (#138).
//!
//! The Notes REST app already exposes documents keyed by id with etag
//! concurrency, so the cache layer is mostly a write-through mirror.
//! Reads are served from the cache so the list paints instantly on
//! launch and stays usable offline; writes go to the server first
//! (because a 412 etag mismatch needs to be surfaced to the UI before
//! we touch local state) and then update the cache on success.
//!
//! Categories are stored as the raw `/`-separated path the server
//! returned.  The sidebar tree-builds at render time so the Joplin
//! WebDAV layout (`Joplin/Notebook A/...`) and flat NC categories
//! (`Work`, `Personal`) both work without schema gymnastics.
//!
//! `apply_notes_delta` is the bulk-upsert entry point used by the
//! background sync.  It mirrors the contacts and calendar deltas:
//! one transaction, all-or-nothing, deletes + upserts in lockstep.

use chrono::Utc;
use rusqlite::params;

use unkai_core::models::Note;

use crate::cache::{Cache, CacheError};

/// Sync bookmark for one account's notes.  We don't have a real
/// sync-token from the Notes API (it's a flat "list everything"
/// endpoint), so this is just a "last successfully synced at"
/// timestamp the UI can render in the header.
#[derive(Debug, Clone, Default)]
pub struct NotesSyncState {
    pub last_synced_at: Option<i64>,
}

impl Cache {
    /// Apply one full-list sync result for an account.
    ///
    /// `upserts` are every note the server currently reports —
    /// this method also computes the set of cached note ids that
    /// the server *didn't* mention this round and deletes them
    /// (server-deleted notes vanish from our cache too).
    ///
    /// Caller-decided semantics so a partial fetch (e.g. one
    /// note's `update_note` response) doesn't have to take this
    /// path; per-row upserts have their own helper below.
    pub fn apply_notes_delta(&self, nc_account_id: &str, notes: &[Note]) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let now = Utc::now().timestamp();

        // Collect server-reported ids so we can compute what to delete.
        let live_ids: std::collections::HashSet<i64> = notes.iter().map(|n| n.id as i64).collect();

        // Drop cache rows that the server didn't return.  Compute the
        // diff in Rust because SQLite doesn't have a clean
        // `WHERE id NOT IN (...)` shape that handles empty sets.
        let cached_ids: Vec<i64> = {
            let mut stmt =
                tx.prepare("SELECT note_id FROM notes WHERE nextcloud_account_id = ?1")?;
            stmt.query_map(params![nc_account_id], |r| r.get::<_, i64>(0))?
                .filter_map(|r| r.ok())
                .collect()
        };
        for id in cached_ids {
            if !live_ids.contains(&id) {
                tx.execute(
                    "DELETE FROM notes WHERE nextcloud_account_id = ?1 AND note_id = ?2",
                    params![nc_account_id, id],
                )?;
            }
        }

        if !notes.is_empty() {
            let mut stmt = tx.prepare(
                "INSERT INTO notes
                    (nextcloud_account_id, note_id, etag, modified_unix,
                     title, category, content, favorite, cached_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (nextcloud_account_id, note_id) DO UPDATE SET
                    etag           = excluded.etag,
                    modified_unix  = excluded.modified_unix,
                    title          = excluded.title,
                    category       = excluded.category,
                    content        = excluded.content,
                    favorite       = excluded.favorite,
                    cached_at      = excluded.cached_at",
            )?;
            for n in notes {
                stmt.execute(params![
                    nc_account_id,
                    n.id as i64,
                    n.etag,
                    n.modified,
                    n.title,
                    n.category,
                    n.content,
                    n.favorite as i64,
                    now,
                ])?;
            }
        }

        // Bump the per-account sync bookmark.
        tx.execute(
            "INSERT INTO notes_sync_state (nextcloud_account_id, last_synced_at)
             VALUES (?1, ?2)
             ON CONFLICT (nextcloud_account_id) DO UPDATE SET last_synced_at = excluded.last_synced_at",
            params![nc_account_id, now],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Single-row upsert used by `create_note` / `update_note` after
    /// the server has acknowledged the change.  Avoids a full delta
    /// pass for the common edit path.
    pub fn upsert_note(&self, note: &Note) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO notes
                (nextcloud_account_id, note_id, etag, modified_unix,
                 title, category, content, favorite, cached_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (nextcloud_account_id, note_id) DO UPDATE SET
                etag           = excluded.etag,
                modified_unix  = excluded.modified_unix,
                title          = excluded.title,
                category       = excluded.category,
                content        = excluded.content,
                favorite       = excluded.favorite,
                cached_at      = excluded.cached_at",
            params![
                note.nextcloud_account_id,
                note.id as i64,
                note.etag,
                note.modified,
                note.title,
                note.category,
                note.content,
                note.favorite as i64,
                now,
            ],
        )?;
        Ok(())
    }

    /// Forget one note locally — used by `delete_note` after the
    /// server has accepted the DELETE.
    pub fn delete_note(&self, nc_account_id: &str, note_id: u64) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM notes WHERE nextcloud_account_id = ?1 AND note_id = ?2",
            params![nc_account_id, note_id as i64],
        )?;
        Ok(())
    }

    /// List every cached note for an account, newest-first.  The
    /// sidebar / list view filter further client-side (by category,
    /// favorite, etc.) — keeping the SQL simple here so future
    /// views don't each grow their own query.
    pub fn list_notes(&self, nc_account_id: &str) -> Result<Vec<Note>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT note_id, etag, modified_unix, title, category, content, favorite
             FROM notes
             WHERE nextcloud_account_id = ?1
             ORDER BY modified_unix DESC",
        )?;
        let rows = stmt.query_map(params![nc_account_id], |r| {
            Ok(Note {
                id: r.get::<_, i64>(0)? as u64,
                nextcloud_account_id: nc_account_id.to_string(),
                etag: r.get(1)?,
                modified: r.get(2)?,
                title: r.get(3)?,
                category: r.get(4)?,
                content: r.get(5)?,
                favorite: r.get::<_, i64>(6)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Pull one note for editing — same row a list call would
    /// return, just keyed by id so the editor doesn't have to scan.
    pub fn get_note(&self, nc_account_id: &str, note_id: u64) -> Result<Option<Note>, CacheError> {
        let conn = self.conn()?;
        let row: Option<Note> = conn
            .query_row(
                "SELECT note_id, etag, modified_unix, title, category, content, favorite
                 FROM notes
                 WHERE nextcloud_account_id = ?1 AND note_id = ?2",
                params![nc_account_id, note_id as i64],
                |r| {
                    Ok(Note {
                        id: r.get::<_, i64>(0)? as u64,
                        nextcloud_account_id: nc_account_id.to_string(),
                        etag: r.get(1)?,
                        modified: r.get(2)?,
                        title: r.get(3)?,
                        category: r.get(4)?,
                        content: r.get(5)?,
                        favorite: r.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .ok();
        Ok(row)
    }

    /// Most recent successful sync for an account.  Powers a "Last
    /// synced 5 m ago" hint in the Notes header.
    pub fn notes_sync_state(&self, nc_account_id: &str) -> Result<NotesSyncState, CacheError> {
        let conn = self.conn()?;
        let row: Option<i64> = conn
            .query_row(
                "SELECT last_synced_at FROM notes_sync_state WHERE nextcloud_account_id = ?1",
                params![nc_account_id],
                |r| r.get(0),
            )
            .ok();
        Ok(NotesSyncState {
            last_synced_at: row,
        })
    }

    /// Wipe all notes for an account — called when the user removes
    /// the NC account so we don't leak stale data.
    pub fn wipe_notes_for_account(&self, nc_account_id: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM notes WHERE nextcloud_account_id = ?1",
            params![nc_account_id],
        )?;
        conn.execute(
            "DELETE FROM notes_sync_state WHERE nextcloud_account_id = ?1",
            params![nc_account_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: u64, title: &str, category: &str, modified: i64) -> Note {
        Note {
            id,
            nextcloud_account_id: "nc1".into(),
            etag: format!("etag-{id}"),
            modified,
            title: title.into(),
            category: category.into(),
            content: format!("# {title}"),
            favorite: false,
        }
    }

    #[test]
    fn delta_upserts_and_deletes() {
        let cache = Cache::open_in_memory().expect("in-memory cache");
        cache
            .apply_notes_delta(
                "nc1",
                &[n(1, "First", "", 100), n(2, "Joplin one", "Joplin", 200)],
            )
            .unwrap();

        let listed = cache.list_notes("nc1").unwrap();
        assert_eq!(listed.len(), 2);
        // Newest-first ordering by modified_unix.
        assert_eq!(listed[0].id, 2);
        assert_eq!(listed[1].id, 1);

        // Second delta drops note 1 and adds note 3.
        cache
            .apply_notes_delta(
                "nc1",
                &[
                    n(2, "Joplin one (edited)", "Joplin", 250),
                    n(3, "Third", "Work", 300),
                ],
            )
            .unwrap();

        let listed = cache.list_notes("nc1").unwrap();
        assert_eq!(listed.len(), 2);
        let ids: Vec<u64> = listed.iter().map(|x| x.id).collect();
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));
        assert!(!ids.contains(&1));
        // Edit landed.
        let two = listed.iter().find(|x| x.id == 2).unwrap();
        assert_eq!(two.title, "Joplin one (edited)");
        assert_eq!(two.modified, 250);
    }

    #[test]
    fn upsert_and_get_round_trip() {
        let cache = Cache::open_in_memory().expect("in-memory cache");
        cache.upsert_note(&n(42, "Solo", "", 999)).unwrap();
        let fetched = cache.get_note("nc1", 42).unwrap().unwrap();
        assert_eq!(fetched.title, "Solo");
        assert_eq!(fetched.modified, 999);
    }

    #[test]
    fn wipe_account_clears_notes_and_sync_state() {
        let cache = Cache::open_in_memory().expect("in-memory cache");
        cache.apply_notes_delta("nc1", &[n(1, "x", "", 1)]).unwrap();
        cache.apply_notes_delta("nc2", &[n(2, "y", "", 2)]).unwrap();

        cache.wipe_notes_for_account("nc1").unwrap();
        assert_eq!(cache.list_notes("nc1").unwrap().len(), 0);
        assert_eq!(cache.list_notes("nc2").unwrap().len(), 1);

        let s = cache.notes_sync_state("nc1").unwrap();
        assert!(s.last_synced_at.is_none());
        let s2 = cache.notes_sync_state("nc2").unwrap();
        assert!(s2.last_synced_at.is_some());
    }
}

//! Local mail cache backed by SQLite.
//!
//! # What lives here
//!
//! - **Envelopes** (light metadata shown in the mail list) and **bodies**
//!   (text/HTML, cached lazily on first open) of messages fetched from IMAP.
//! - **Folder listings** and **per-folder sync state** so the next launch
//!   can start displaying something before the network is touched.
//!
//! # What does not live here
//!
//! - **Passwords**: always in the OS keychain (`credentials.rs`). Never
//!   the DB, never disk.
//!
//! # Read strategy
//!
//! The UI loads from the cache first (instant, offline-safe) and then
//! kicks off a network refresh which write-throughs back to the cache.
//! The Tauri layer owns this dance — there are separate `get_cached_*`
//! commands for the cache path and `fetch_*` commands for the network
//! path. Keeping them distinct makes the strategy explicit in the UI
//! and lets future views (search, notifications) pick whichever they
//! need.
//!
//! # Thread-safety
//!
//! The cache holds an `r2d2` pool internally. Every method checks out a
//! connection, does its work, and returns it. The pool is internally
//! synchronised so `Cache` is cheap to `clone()` and share across tasks.

pub mod calendars;
pub mod contacts;
pub mod key;
pub mod notes;
pub mod pool;
pub mod schema;
pub mod search;

pub use calendars::{
    CachedCalendar, CalendarEventRow, CalendarEventServerHandle, CalendarRow, CalendarSyncState,
    ExpansionInput,
};
pub use contacts::{AddressbookSyncState, ContactRow, ContactServerHandle};
pub use notes::NotesSyncState;
pub use search::{SearchFilters, SearchHit, SearchScope};

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, TimeZone, Utc};
use nimbus_core::NimbusError;
use nimbus_core::models::{Email, EmailEnvelope, Folder};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{OptionalExtension, params};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::cache::pool::SqlitePool;

/// Errors specific to the cache layer. Converted to `NimbusError::Storage`
/// when crossing out of the crate so the rest of the app doesn't have to
/// care which database we happen to be using.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("failed to open cache: {0}")]
    Open(String),
    #[error("migration failed: {0}")]
    Migration(String),
    #[error("sqlite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("pool error: {0}")]
    Pool(#[from] r2d2::Error),
    /// The cache is in FIDO-only mode (#164) and the user hasn't
    /// authenticated yet.  Surface this from any IPC that touches
    /// cached data so the UI can hold off until the lock screen
    /// completes the unlock.
    #[error("cache is locked — authenticate to unlock")]
    Locked,
}

impl From<CacheError> for NimbusError {
    fn from(e: CacheError) -> Self {
        NimbusError::Storage(e.to_string())
    }
}

/// Cached per-folder sync bookmark.
///
/// `uidvalidity` is the IMAP server's guarantee that existing UIDs in the
/// folder are still valid. If the server ever returns a different value,
/// we must throw away everything cached for that folder and start over.
#[derive(Debug, Clone)]
pub struct SyncState {
    pub uidvalidity: Option<u32>,
    pub highest_uid_seen: Option<u32>,
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// Handle to the local mail cache. Cheap to clone — under the hood it's
/// an `Arc` around a connection pool.
///
/// The pool is `Option`-wrapped so the cache can exist in a
/// **locked** state (#164 Phase 1B): if the keychain envelope has
/// no plain master key, `Cache::open_default` returns a Cache
/// whose pool is `None` and every data-touching method returns
/// `CacheError::Locked` until `unlock_with_master_key` is called
/// from the unlock-flow IPCs.
#[derive(Clone)]
pub struct Cache {
    pool: Arc<RwLock<Option<SqlitePool>>>,
    /// Where the encrypted DB lives on disk.  Held so `unlock`
    /// can open the pool without re-resolving the path.
    path: PathBuf,
    /// In-memory copy of the SQLCipher master key (64-char
    /// lowercase hex), populated after a successful unlock.  Lets
    /// `disable_fido_only_mode` write the key back into the
    /// keychain envelope without having to re-prompt the user.
    /// `None` while the cache is locked.
    master_key_hex: Arc<RwLock<Option<String>>>,
}

impl Cache {
    /// Open the app's default cache location:
    /// `<config-dir>/nimbus-mail/cache.db`, and run any pending migrations.
    ///
    /// The DB is encrypted via SQLCipher; the master key is fetched from
    /// (or freshly generated in) the OS keychain. See `key.rs`.
    pub fn open_default() -> Result<Self, NimbusError> {
        let path = default_cache_path()?;
        // Honour the keychain envelope: when the user has flipped
        // the cache into FIDO-only mode there's no plain key
        // available, and we return a *locked* Cache whose pool
        // stays `None` until `unlock_with_master_key` is called
        // from the unlock IPCs.
        let envelope = key::load_envelope()?;
        match envelope.plain_key.as_deref() {
            Some(hex) if hex.len() == 64 => {
                Self::open_with_key(&path, hex.to_string()).map_err(Into::into)
            }
            Some(hex) => Err(NimbusError::Storage(format!(
                "unexpected master key length: {} chars (expected 64)",
                hex.len()
            ))),
            None => {
                // No keychain entry yet — first-run; mint a key and
                // open normally.  `get_or_create_master_key`
                // handles the empty-keychain case for us.
                if envelope.wraps.is_empty() {
                    let key_hex = key::get_or_create_master_key()?;
                    Self::open_with_key(&path, key_hex).map_err(Into::into)
                } else {
                    info!(
                        "Cache is in FIDO-only mode ({} registered methods); \
                         pool stays locked until unlock IPC runs",
                        envelope.wraps.len()
                    );
                    Ok(Self {
                        pool: Arc::new(RwLock::new(None)),
                        path,
                        master_key_hex: Arc::new(RwLock::new(None)),
                    })
                }
            }
        }
    }

    /// Open a cache at an explicit path with a caller-supplied key.
    ///
    /// Used by the default opener above and by future multi-profile
    /// support. The key must be a 64-char lowercase hex string.
    ///
    /// Handles the pre-encryption → encryption upgrade: if a legacy
    /// unencrypted `cache.db` is found on disk, opening it with a key
    /// will fail at the first decrypt; we detect that, wipe the file,
    /// and recreate from scratch. The user loses their cache but
    /// re-sync fills it back in on next launch.
    pub fn open_with_key(path: &Path, key_hex: String) -> Result<Self, CacheError> {
        info!("Opening encrypted mail cache at {}", path.display());
        let pool = match pool::open_pool(path, key_hex.clone()) {
            Ok(p) => p,
            Err(e) if is_wrong_key_error(&e) && path.exists() => {
                warn!(
                    "Existing cache at {} could not be unlocked (likely an \
                     unencrypted cache from a pre-encryption build). Wiping \
                     and recreating — mail will re-sync on next launch.",
                    path.display()
                );
                wipe_cache_files(path)?;
                pool::open_pool(path, key_hex.clone())?
            }
            Err(e) => return Err(e),
        };
        // Run migrations on a freshly checked-out connection so the pool
        // is available for use right after this call returns.
        let mut conn = pool.get()?;
        schema::run_migrations(&mut conn)?;
        // Sweep stale optimistic-action tombstones from a crashed
        // previous run (#174).  Anything still flagged as
        // `pending_action` at startup belongs to a process that's
        // already gone, so the IMAP request never completed and the
        // safe move is to make those rows visible again.
        if let Err(e) = conn.execute(
            "UPDATE messages SET pending_action = NULL
             WHERE pending_action IS NOT NULL",
            [],
        ) {
            warn!("startup pending-action sweep failed: {e}");
        }
        Ok(Self {
            pool: Arc::new(RwLock::new(Some(pool))),
            path: path.to_path_buf(),
            master_key_hex: Arc::new(RwLock::new(Some(key_hex))),
        })
    }

    /// True when the pool isn't open yet — every data method
    /// returns `Locked` until `unlock_with_master_key` runs.
    pub fn is_locked(&self) -> bool {
        self.pool.read().map(|g| g.is_none()).unwrap_or(true)
    }

    /// Open the pool for a previously-locked Cache.  Called from
    /// the unlock-flow IPCs once the user has authenticated and
    /// the master key has been recovered from a wrap.
    /// Idempotent — a second call with the same key is a no-op.
    pub fn unlock_with_master_key(&self, key_hex: String) -> Result<(), CacheError> {
        if !self.is_locked() {
            return Ok(());
        }
        // No wipe-on-wrong-key fallback here.  At unlock time a
        // SQLCipher key mismatch means authentication failed —
        // silently wiping the DB would destroy the user's mail and
        // accounts.  Surface the error so the unlock IPC can
        // re-prompt instead.  (The legacy-DB wipe lives in
        // `open_with_key`, which only runs on first boot when no
        // wraps exist.)
        let pool = pool::open_pool(&self.path, key_hex.clone())?;
        let mut conn = pool.get()?;
        schema::run_migrations(&mut conn)?;
        // Same stale-tombstone sweep as `open_with_key` — see
        // there for the why.  Done while we still hold the pooled
        // conn so the cleanup runs before any data IPC can.
        if let Err(e) = conn.execute(
            "UPDATE messages SET pending_action = NULL
             WHERE pending_action IS NOT NULL",
            [],
        ) {
            warn!("post-unlock pending-action sweep failed: {e}");
        }
        drop(conn);
        let mut guard = self.pool.write().expect("Cache pool RwLock poisoned");
        *guard = Some(pool);
        // Stash the recovered key so `disable_fido_only_mode` can
        // write it back into the keychain envelope without making
        // the user re-authenticate.  Cleared in any future
        // re-lock path.
        let mut key_guard = self
            .master_key_hex
            .write()
            .expect("Cache master_key_hex RwLock poisoned");
        *key_guard = Some(key_hex);
        Ok(())
    }

    /// Read the in-memory copy of the SQLCipher master key (hex).
    /// Returns `None` while the cache is locked or for an
    /// in-memory test cache.  Used by `disable_fido_only_mode`
    /// to restore `envelope.plain_key` without re-prompting.
    pub fn master_key_hex(&self) -> Option<String> {
        self.master_key_hex.read().ok().and_then(|g| g.clone())
    }

    /// Delete the cache DB and its WAL sidecars from disk.  Used
    /// by the "wipe on failed authentication" policy: when the
    /// user exhausts their unlock attempts we drop the file so
    /// the next launch starts clean.  The Cache stays locked
    /// (pool is `None` either way) — the caller is responsible
    /// for clearing the keychain envelope's wraps if it wants a
    /// completely fresh setup on next launch.
    pub fn wipe_on_disk(&self) -> Result<(), CacheError> {
        wipe_cache_files(&self.path)
    }

    /// Borrow a pooled connection or return `Locked`.  Every
    /// data-touching method funnels through here so locked
    /// state propagates uniformly.  `pub(crate)` so sibling
    /// modules (`account_store`, …) can reuse it instead of
    /// duplicating the lock-and-checkout dance.
    pub(crate) fn conn(&self) -> Result<PooledConnection<SqliteConnectionManager>, CacheError> {
        let guard = self.pool.read().expect("Cache pool RwLock poisoned");
        let pool = guard.as_ref().ok_or(CacheError::Locked)?;
        Ok(pool.get()?)
    }

    /// Open an in-memory cache for tests. Each call gets its own
    /// fresh DB — see `pool::open_memory_pool` for the URI trick
    /// that makes that work. Useful for any sibling module
    /// (e.g. `account_store`) that needs a Cache to run unit tests
    /// against without touching the user's real config dir or the
    /// keychain.
    pub fn open_in_memory() -> Result<Self, CacheError> {
        let pool = pool::open_memory_pool()?;
        let mut conn = pool.get()?;
        schema::run_migrations(&mut conn)?;
        drop(conn);
        Ok(Self {
            pool: Arc::new(RwLock::new(Some(pool))),
            path: PathBuf::from(":memory:"),
            master_key_hex: Arc::new(RwLock::new(None)),
        })
    }

    /// Drop every cached row whose `account_id` isn't in `active_ids`.
    ///
    /// Called on app startup as a defense-in-depth scrub for the case
    /// where `wipe_account` at removal time didn't run (crash, disk
    /// error, older build without the wipe) or where an account was
    /// re-added under a fresh UUID leaving the old id's rows behind.
    /// Unified-inbox views would otherwise hand the UI envelopes
    /// whose owning account no longer exists and `load_account`
    /// would fail on every click.
    ///
    /// Returns the count of orphan account ids that were pruned —
    /// zero on a clean cache, any other number is worth a log line.
    pub fn prune_orphan_accounts(&self, active_ids: &[String]) -> Result<usize, CacheError> {
        let conn = self.conn()?;
        // Collect every distinct account_id across the three tables
        // that might hold orphans. Using a union keeps this robust
        // against one table drifting ahead of another (e.g. a past
        // bug only cleaning `messages` on removal).
        let mut stmt = conn.prepare(
            "SELECT account_id FROM messages
             UNION
             SELECT account_id FROM folders
             UNION
             SELECT account_id FROM folder_sync_state",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let active: std::collections::HashSet<&str> =
            active_ids.iter().map(String::as_str).collect();
        let orphans: Vec<String> = rows
            .filter_map(Result::ok)
            .filter(|id| !active.contains(id.as_str()))
            .collect();
        drop(stmt);
        for id in &orphans {
            // Reuse `wipe_account`'s three DELETE statements so any
            // tables it learns about in the future are automatically
            // covered by the scrub too.
            self.wipe_account(id)?;
        }
        if !orphans.is_empty() {
            warn!(
                "Pruned {} orphan account id(s) from cache: {:?}",
                orphans.len(),
                orphans
            );
        }
        Ok(orphans.len())
    }

    /// Clears the cache for a specific account — called when an account
    /// is removed, or when `UIDVALIDITY` changes and we need to start fresh.
    pub fn wipe_account(&self, account_id: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        // `ON DELETE CASCADE` on message_bodies means deleting from
        // messages clears the bodies too. folders / folder_sync_state
        // don't have FKs, so we clear them explicitly.
        conn.execute("DELETE FROM messages WHERE account_id = ?1", [account_id])?;
        conn.execute("DELETE FROM folders WHERE account_id = ?1", [account_id])?;
        conn.execute(
            "DELETE FROM folder_sync_state WHERE account_id = ?1",
            [account_id],
        )?;
        info!("Wiped cache entries for account '{account_id}'");
        Ok(())
    }

    /// Carry every cached row for a folder over to a new folder name
    /// in lockstep with an IMAP `RENAME`. The server preserves UIDs
    /// across a rename, so updating the `folder` column on `messages`,
    /// `folder_sync_state`, and `folders` is enough to keep every
    /// envelope / body / unread-count bookmark pointing at the right
    /// mailbox — without re-fetching a single byte.
    pub fn rename_folder(
        &self,
        account_id: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE messages SET folder = ?3
             WHERE account_id = ?1 AND folder = ?2",
            params![account_id, old_name, new_name],
        )?;
        conn.execute(
            "UPDATE folder_sync_state SET folder = ?3
             WHERE account_id = ?1 AND folder = ?2",
            params![account_id, old_name, new_name],
        )?;
        conn.execute(
            "UPDATE folders SET name = ?3
             WHERE account_id = ?1 AND name = ?2",
            params![account_id, old_name, new_name],
        )?;
        info!("Renamed cache rows: '{account_id}'/'{old_name}' -> '{new_name}'");
        Ok(())
    }

    /// Clear all cached rows for a single folder — used when the server's
    /// `UIDVALIDITY` for that folder has changed, meaning every UID we had
    /// cached now refers to a different message (or none at all).
    ///
    /// `ON DELETE CASCADE` handles the bodies; we explicitly drop the
    /// `folder_sync_state` row too so the next sync starts from scratch.
    pub fn wipe_folder(&self, account_id: &str, folder: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM messages WHERE account_id = ?1 AND folder = ?2",
            params![account_id, folder],
        )?;
        conn.execute(
            "DELETE FROM folder_sync_state WHERE account_id = ?1 AND folder = ?2",
            params![account_id, folder],
        )?;
        info!("Wiped cache for '{account_id}' / '{folder}' (UIDVALIDITY reset)");
        Ok(())
    }

    // ── Folders ─────────────────────────────────────────────────

    /// Replace the cached folder list for an account.
    ///
    /// Folder names can change (user renames, server-side mailbox removal),
    /// so we wipe-and-reinsert inside a transaction rather than trying to
    /// diff. The folder list is small (dozens of rows at most) so this
    /// is effectively free.
    pub fn upsert_folders(&self, account_id: &str, folders: &[Folder]) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM folders WHERE account_id = ?1", [account_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO folders (account_id, name, delimiter, attributes, unread_count)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for f in folders {
                let attrs = serde_json::to_string(&f.attributes).unwrap_or_else(|_| "[]".into());
                stmt.execute(params![
                    account_id,
                    f.name,
                    f.delimiter,
                    attrs,
                    f.unread_count,
                ])?;
            }
        }
        tx.commit()?;
        debug!(
            "Cached {} folders for account '{account_id}'",
            folders.len()
        );
        Ok(())
    }

    /// Read the cached folder list for an account.
    ///
    /// Returns folders in the order they were inserted — i.e. the
    /// server's native order, which is what the user expects (INBOX
    /// first, then the server's own ordering). `upsert_folders`
    /// wipes-and-reinserts in a single transaction, so SQLite's
    /// monotonically-assigned `rowid` matches the input iteration
    /// order exactly. Sorting by `name` instead — as we used to —
    /// alphabetised by ASCII code, which puts all-caps `INBOX`
    /// behind names like `Drafts` and made the sidebar look
    /// scrambled compared to every other mail client.
    pub fn get_folders(&self, account_id: &str) -> Result<Vec<Folder>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, delimiter, attributes, unread_count
             FROM folders
             WHERE account_id = ?1
             ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            let attrs_json: String = r.get(2)?;
            let attributes: Vec<String> = serde_json::from_str(&attrs_json).unwrap_or_default();
            let unread: Option<i64> = r.get(3)?;
            Ok(Folder {
                name: r.get(0)?,
                delimiter: r.get(1)?,
                attributes,
                unread_count: unread.map(|v| v as u32),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ── Envelopes ───────────────────────────────────────────────

    /// Upsert a batch of envelopes, tagging each with the given `account_id`.
    ///
    /// `EmailEnvelope` doesn't carry an account id — the frontend never
    /// needs it, and the Tauri command always knows which account it
    /// connected to. We take the id once here instead of widening the
    /// shared struct.
    ///
    /// Uses `ON CONFLICT ... DO UPDATE` so re-fetching an existing message
    /// refreshes its flags (e.g. user marked-as-read on another device).
    /// Runs inside a transaction: either the whole batch lands or none
    /// of it does.
    pub fn upsert_envelopes_for_account(
        &self,
        account_id: &str,
        envelopes: &[EmailEnvelope],
    ) -> Result<(), CacheError> {
        if envelopes.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let now = Utc::now().timestamp();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages
                   (account_id, folder, uid, from_addr, subject, internal_date,
                    is_read, is_starred, cached_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (account_id, folder, uid) DO UPDATE SET
                   from_addr     = excluded.from_addr,
                   subject       = excluded.subject,
                   internal_date = excluded.internal_date,
                   is_read       = excluded.is_read,
                   is_starred    = excluded.is_starred,
                   cached_at     = excluded.cached_at",
            )?;
            for env in envelopes {
                stmt.execute(params![
                    account_id,
                    env.folder,
                    env.uid as i64,
                    env.from,
                    env.subject,
                    env.date.timestamp(),
                    env.is_read as i64,
                    env.is_starred as i64,
                    now,
                ])?;
            }
        }
        tx.commit()?;
        debug!(
            "Cached {} envelopes for '{account_id}' (first folder: {})",
            envelopes.len(),
            envelopes.first().map(|e| e.folder.as_str()).unwrap_or("-"),
        );
        Ok(())
    }

    /// Return the newest `limit` envelopes in a folder from the cache.
    ///
    /// Uses the `messages_by_folder_date` index to avoid a sort.
    pub fn get_envelopes(
        &self,
        account_id: &str,
        folder: &str,
        limit: u32,
    ) -> Result<Vec<EmailEnvelope>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT uid, folder, from_addr, subject, internal_date, is_read, is_starred
             FROM messages
             WHERE account_id = ?1 AND folder = ?2 AND pending_action IS NULL
             ORDER BY internal_date DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, folder, limit as i64], |r| {
            let ts: i64 = r.get(4)?;
            let date = Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now);
            Ok(EmailEnvelope {
                uid: r.get::<_, i64>(0)? as u32,
                folder: r.get(1)?,
                from: r.get(2)?,
                subject: r.get(3)?,
                date,
                is_read: r.get::<_, i64>(5)? != 0,
                is_starred: r.get::<_, i64>(6)? != 0,
                account_id: account_id.to_string(),
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Return the newest `limit` envelopes in `folder` whose body has
    /// **not** yet been fetched (no row in `message_bodies`).  Used by
    /// the launch-time prerender (#178) to warm the message cache —
    /// the user clicks an inbox row and the reading pane paints
    /// instantly because the body is already on disk, instead of
    /// waiting for an IMAP round-trip.
    pub fn get_envelopes_missing_body(
        &self,
        account_id: &str,
        folder: &str,
        limit: u32,
    ) -> Result<Vec<u32>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT m.uid
             FROM messages m
             LEFT JOIN message_bodies b USING (account_id, folder, uid)
             WHERE m.account_id = ?1
               AND m.folder = ?2
               AND m.pending_action IS NULL
               AND b.uid IS NULL
             ORDER BY m.internal_date DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, folder, limit as i64], |r| {
            Ok(r.get::<_, i64>(0)? as u32)
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Return the newest `limit` envelopes in `folder` across **all**
    /// accounts. Powers the unified-inbox view: each row carries its
    /// owning `account_id` so the UI can render an account label and
    /// route the "open message" click to the right account.
    pub fn get_unified_envelopes(
        &self,
        folder: &str,
        limit: u32,
    ) -> Result<Vec<EmailEnvelope>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT account_id, uid, folder, from_addr, subject, internal_date, is_read, is_starred
             FROM messages
             WHERE folder = ?1 AND pending_action IS NULL
             ORDER BY internal_date DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![folder, limit as i64], |r| {
            let ts: i64 = r.get(5)?;
            let date = Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now);
            Ok(EmailEnvelope {
                account_id: r.get(0)?,
                uid: r.get::<_, i64>(1)? as u32,
                folder: r.get(2)?,
                from: r.get(3)?,
                subject: r.get(4)?,
                date,
                is_read: r.get::<_, i64>(6)? != 0,
                is_starred: r.get::<_, i64>(7)? != 0,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Mark a cached envelope as read (sets `is_read = 1`) and keep
    /// the folder's `unread_count` in sync by decrementing it iff the
    /// message was previously unread.
    ///
    /// Used by the "mark as read when opened" path: we flip the local
    /// cache immediately so the UI reflects the change without waiting
    /// for the network round-trip to the IMAP server. If the row isn't
    /// cached yet (message was never listed), the message-table UPDATE
    /// is a no-op and we don't decrement the folder count — there's
    /// nothing to subtract from.
    ///
    /// Wrapped in a transaction so the message flip and the folder
    /// count adjustment land atomically; an interrupted call can never
    /// leave `is_read = 1` next to an unchanged `unread_count`.
    pub fn mark_envelope_read(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let was_unread: bool = tx
            .query_row(
                "SELECT is_read = 0 FROM messages
                 WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
                params![account_id, folder, uid as i64],
                |r| r.get::<_, i64>(0).map(|v| v != 0),
            )
            .unwrap_or(false);

        tx.execute(
            "UPDATE messages SET is_read = 1
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64],
        )?;

        if was_unread {
            // `MAX(unread_count - 1, 0)` guards against an off-by-one
            // dropping below zero when the cached folder count is
            // already stale (e.g. another client read the message,
            // a background poll lowered `unread_count`, then we read
            // it ourselves).
            tx.execute(
                "UPDATE folders
                 SET unread_count = MAX(COALESCE(unread_count, 0) - 1, 0)
                 WHERE account_id = ?1 AND name = ?2",
                params![account_id, folder],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Return every cached envelope UID for a folder — used by the
    /// reconciler that diffs the cache against the server's live UID
    /// set after each incremental fetch and drops rows whose UIDs no
    /// longer exist on the server.
    pub fn list_envelope_uids(
        &self,
        account_id: &str,
        folder: &str,
    ) -> Result<Vec<u32>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT uid FROM messages
             WHERE account_id = ?1 AND folder = ?2",
        )?;
        let rows = stmt.query_map(params![account_id, folder], |r| r.get::<_, i64>(0))?;
        let mut uids = Vec::new();
        for row in rows {
            uids.push(row? as u32);
        }
        Ok(uids)
    }

    /// Mark a cached envelope as having an in-flight optimistic
    /// action so envelope-list queries hide it instantly (#174).
    /// `action` is a free-form string — `"delete"` for delete /
    /// move-to-trash, `"move:<dest>"` for explicit folder moves.
    /// Cleared on IMAP failure via `clear_message_pending`; on
    /// IMAP success the existing `remove_envelope` / source-folder
    /// move cleanup drops the row entirely, so the pending flag
    /// quietly disappears with it.
    pub fn mark_message_pending(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        action: &str,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE messages SET pending_action = ?4
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64, action],
        )?;
        Ok(())
    }

    /// Reverse of `mark_message_pending` — called when the IMAP
    /// action errors so the row reappears in the next list pull
    /// without the user having to restart anything.
    pub fn clear_message_pending(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE messages SET pending_action = NULL
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64],
        )?;
        Ok(())
    }

    /// Wipe every leftover `pending_action` tombstone.  Called on
    /// app startup (after `unlock_with_master_key` opens the pool)
    /// so a row left tombstoned by a crashed run doesn't stay
    /// permanently invisible.  At launch nothing is genuinely in
    /// flight — the IMAP requests live in the previous process —
    /// so any surviving pending flag is by definition stale.
    /// Returns the number of rows reset.
    pub fn clear_all_pending_actions(&self) -> Result<usize, CacheError> {
        let conn = self.conn()?;
        let n = conn.execute(
            "UPDATE messages SET pending_action = NULL
             WHERE pending_action IS NOT NULL",
            [],
        )?;
        Ok(n)
    }

    /// Remove a single cached envelope + body after the message has been
    /// expunged / moved on the server. The incremental envelope fetch
    /// only pulls UIDs `> highest_seen`, so without an explicit delete
    /// here the cache accumulates ghost rows for every expunged UID —
    /// and MailList keeps showing them, handing the user stale UIDs
    /// that the server has since reassigned or reclaimed.
    ///
    /// If the envelope was unread at the time of removal, the folder
    /// `unread_count` is also decremented so the sidebar badge tracks
    /// the row disappearing. Same clamp-at-zero guard as
    /// `mark_envelope_read`. Returns `true` iff a row was actually
    /// removed — callers can tell the difference between "cleaned up
    /// a real stale row" and "no row existed in the first place".
    pub fn remove_envelope(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<bool, CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let was_unread: bool = tx
            .query_row(
                "SELECT is_read = 0 FROM messages
                 WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
                params![account_id, folder, uid as i64],
                |r| r.get::<_, i64>(0).map(|v| v != 0),
            )
            .unwrap_or(false);

        let rows = tx.execute(
            "DELETE FROM messages
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64],
        )?;

        if rows > 0 && was_unread {
            tx.execute(
                "UPDATE folders
                 SET unread_count = MAX(COALESCE(unread_count, 0) - 1, 0)
                 WHERE account_id = ?1 AND name = ?2",
                params![account_id, folder],
            )?;
        }

        tx.commit()?;
        Ok(rows > 0)
    }

    /// Mark a cached envelope as unread (sets `is_read = 0`) and keep
    /// the folder's `unread_count` in sync by incrementing it iff the
    /// message was previously read. Mirror of `mark_envelope_read`.
    pub fn mark_envelope_unread(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let was_read: bool = tx
            .query_row(
                "SELECT is_read = 1 FROM messages
                 WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
                params![account_id, folder, uid as i64],
                |r| r.get::<_, i64>(0).map(|v| v != 0),
            )
            .unwrap_or(false);

        tx.execute(
            "UPDATE messages SET is_read = 0
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64],
        )?;

        if was_read {
            tx.execute(
                "UPDATE folders
                 SET unread_count = COALESCE(unread_count, 0) + 1
                 WHERE account_id = ?1 AND name = ?2",
                params![account_id, folder],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Bump a folder's `unread_count` by `delta` (positive to add,
    /// negative to subtract). Treats a `NULL` stored count as `0`.
    /// Used by the poll path to credit newly-arrived unread mail
    /// against the badge without waiting for a fresh `STATUS` round-trip.
    pub fn bump_folder_unread(
        &self,
        account_id: &str,
        folder: &str,
        delta: i64,
    ) -> Result<(), CacheError> {
        if delta == 0 {
            return Ok(());
        }
        let conn = self.conn()?;
        conn.execute(
            "UPDATE folders
             SET unread_count = MAX(COALESCE(unread_count, 0) + ?3, 0)
             WHERE account_id = ?1 AND name = ?2",
            params![account_id, folder, delta],
        )?;
        Ok(())
    }

    /// Total unread messages across all accounts' INBOX folders.
    ///
    /// Feeds the tray tooltip ("Nimbus Mail — 3 unread") and any
    /// aggregate badge UI. We scope to INBOX only because other folders
    /// (Archive, Trash) aren't typically surfaced as "unread" to the
    /// user even when they technically have `is_read = 0` rows.
    pub fn total_unread_count(&self) -> Result<u32, CacheError> {
        let conn = self.conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages
             WHERE folder = 'INBOX' AND is_read = 0 AND pending_action IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(count as u32)
    }

    /// Per-account unread INBOX count, keyed by `account_id`
    /// (issue #115).  Used by the IconRail to paint a red badge
    /// on the avatar of each account that has new mail.
    /// Accounts with zero unread messages are *omitted* from the
    /// map so the caller can `?? 0` without the row showing up
    /// as "0 unread" in the UI.
    pub fn unread_counts_by_account(
        &self,
    ) -> Result<std::collections::HashMap<String, u32>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT account_id, COUNT(*) FROM messages
             WHERE folder = 'INBOX' AND is_read = 0 AND pending_action IS NULL
             GROUP BY account_id",
        )?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let n: i64 = r.get(1)?;
            Ok((id, n as u32))
        })?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (id, n) = r?;
            if n > 0 {
                out.insert(id, n);
            }
        }
        Ok(out)
    }

    // ── Message bodies ──────────────────────────────────────────

    /// Upsert a cached message body alongside its envelope.
    ///
    /// Takes an `Email` since that's the shape the IMAP client returns — we
    /// split it into an envelope row (via `upsert_envelopes_for_account`)
    /// and a body row here, in a single transaction so partial rows never
    /// survive a failed write.
    pub fn upsert_message(&self, email: &Email) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let now = Utc::now().timestamp();

        // Envelope row — mirrors upsert_envelopes_for_account but inside
        // the same transaction as the body so the two can't drift.
        tx.execute(
            "INSERT INTO messages
                (account_id, folder, uid, from_addr, subject, internal_date,
                 is_read, is_starred, cached_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (account_id, folder, uid) DO UPDATE SET
                from_addr     = excluded.from_addr,
                subject       = excluded.subject,
                internal_date = excluded.internal_date,
                is_read       = excluded.is_read,
                is_starred    = excluded.is_starred,
                cached_at     = excluded.cached_at",
            params![
                email.account_id,
                email.folder,
                // `id` from IMAP is formatted as "folder:uid" in the
                // fetch path — we don't rely on it here, the UID is
                // re-parsed by the caller.
                uid_from_email_id(&email.id) as i64,
                email.from,
                email.subject,
                email.date.timestamp(),
                email.is_read as i64,
                email.is_starred as i64,
                now,
            ],
        )?;

        // Addresses are stored as JSON arrays — see the v1 → v2 migration
        // note. `unwrap_or_else` fallbacks are defensive; serde_json on a
        // Vec<String> can only fail if allocation fails.
        let to_json = serde_json::to_string(&email.to).unwrap_or_else(|_| "[]".into());
        let cc_json = serde_json::to_string(&email.cc).unwrap_or_else(|_| "[]".into());
        // Attachment metadata as JSON — one record per attachment,
        // with the stable `part_id` the IMAP re-fetch uses. See v5 → v6.
        let attachments_json =
            serde_json::to_string(&email.attachments).unwrap_or_else(|_| "[]".into());

        tx.execute(
            "INSERT INTO message_bodies
                (account_id, folder, uid, body_text, body_html,
                 has_attachments, raw_size, cached_at, to_addrs, cc_addrs,
                 attachments)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT (account_id, folder, uid) DO UPDATE SET
                body_text       = excluded.body_text,
                body_html       = excluded.body_html,
                has_attachments = excluded.has_attachments,
                raw_size        = excluded.raw_size,
                cached_at       = excluded.cached_at,
                to_addrs        = excluded.to_addrs,
                cc_addrs        = excluded.cc_addrs,
                attachments     = excluded.attachments",
            params![
                email.account_id,
                email.folder,
                uid_from_email_id(&email.id) as i64,
                email.body_text,
                email.body_html,
                email.has_attachments as i64,
                None::<i64>,
                now,
                to_json,
                cc_json,
                attachments_json,
            ],
        )?;
        tx.commit()?;
        debug!(
            "Cached message {}:{}:{} (text={}, html={}, atts={})",
            email.account_id,
            email.folder,
            uid_from_email_id(&email.id),
            email.body_text.is_some(),
            email.body_html.is_some(),
            email.has_attachments,
        );
        Ok(())
    }

    /// Look up a fully-hydrated cached message.
    ///
    /// Joins `messages` and `message_bodies`; returns `None` if we haven't
    /// fetched the body yet (envelope-only is not enough to render MailView,
    /// so the caller should treat envelope-only as "not cached" and go to
    /// the network).
    pub fn get_message(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<Option<Email>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT m.from_addr, m.subject, m.internal_date,
                        m.is_read, m.is_starred,
                        b.body_text, b.body_html, b.has_attachments,
                        b.to_addrs, b.cc_addrs, b.attachments
                 FROM messages m
                 INNER JOIN message_bodies b USING (account_id, folder, uid)
                 WHERE m.account_id = ?1 AND m.folder = ?2 AND m.uid = ?3",
                params![account_id, folder, uid as i64],
                |r| {
                    let ts: i64 = r.get(2)?;
                    let date = Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now);
                    let to_json: String = r.get(8)?;
                    let cc_json: String = r.get(9)?;
                    let attachments_json: String = r.get(10)?;
                    Ok(Email {
                        id: format!("{folder}:{uid}"),
                        account_id: account_id.to_string(),
                        folder: folder.to_string(),
                        from: r.get(0)?,
                        to: serde_json::from_str(&to_json).unwrap_or_default(),
                        cc: serde_json::from_str(&cc_json).unwrap_or_default(),
                        subject: r.get(1)?,
                        body_text: r.get(5)?,
                        body_html: r.get(6)?,
                        date,
                        is_read: r.get::<_, i64>(3)? != 0,
                        is_starred: r.get::<_, i64>(4)? != 0,
                        has_attachments: r.get::<_, i64>(7)? != 0,
                        attachments: serde_json::from_str(&attachments_json).unwrap_or_default(),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    // ── Sync state ──────────────────────────────────────────────

    pub fn get_sync_state(
        &self,
        account_id: &str,
        folder: &str,
    ) -> Result<Option<SyncState>, CacheError> {
        let conn = self.conn()?;
        let state = conn
            .query_row(
                "SELECT uidvalidity, highest_uid_seen, last_synced_at
                 FROM folder_sync_state
                 WHERE account_id = ?1 AND folder = ?2",
                params![account_id, folder],
                |r| {
                    let ts: Option<i64> = r.get(2)?;
                    let uv: Option<i64> = r.get(0)?;
                    let hi: Option<i64> = r.get(1)?;
                    Ok(SyncState {
                        uidvalidity: uv.map(|v| v as u32),
                        highest_uid_seen: hi.map(|v| v as u32),
                        last_synced_at: ts.and_then(|t| Utc.timestamp_opt(t, 0).single()),
                    })
                },
            )
            .optional()?;
        Ok(state)
    }

    pub fn set_sync_state(
        &self,
        account_id: &str,
        folder: &str,
        state: &SyncState,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO folder_sync_state
                (account_id, folder, uidvalidity, highest_uid_seen, last_synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (account_id, folder) DO UPDATE SET
                uidvalidity      = excluded.uidvalidity,
                highest_uid_seen = excluded.highest_uid_seen,
                last_synced_at   = excluded.last_synced_at",
            params![
                account_id,
                folder,
                state.uidvalidity.map(|v| v as i64),
                state.highest_uid_seen.map(|v| v as i64),
                state.last_synced_at.map(|t| t.timestamp()),
            ],
        )?;
        Ok(())
    }

    // ── Attachment-thumbnail cache (#157) ──────────────────────
    //
    // MailView's chip strip used to re-fetch every image / video
    // attachment per open and re-extract its thumbnail.  These
    // helpers persist a tiny JPEG (≤256 px on the long edge)
    // generated by the frontend so subsequent opens render
    // straight from the cache without an IPC, blob copy, or
    // GStreamer pipeline.

    /// Insert / replace a stored thumbnail for one attachment.
    /// `bytes` is whatever encoded image format the frontend
    /// produced — we treat it opaquely and hand it back to
    /// callers verbatim.
    pub fn put_attachment_preview(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        part_id: u32,
        mime: &str,
        bytes: &[u8],
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO attachment_previews
                 (account_id, folder, uid, part_id, mime, bytes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'))",
            params![account_id, folder, uid as i64, part_id as i64, mime, bytes],
        )?;
        Ok(())
    }

    /// Load every stored thumbnail for one message in a single
    /// query — MailView batches them all into the in-memory
    /// thumb cache when the email mounts.
    pub fn get_attachment_previews_for_message(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<Vec<AttachmentPreview>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT part_id, mime, bytes
             FROM attachment_previews
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
        )?;
        let rows = stmt.query_map(params![account_id, folder, uid as i64], |r| {
            Ok(AttachmentPreview {
                part_id: r.get::<_, i64>(0)? as u32,
                mime: r.get(1)?,
                bytes: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

/// One stored attachment thumbnail.  Returned from
/// `get_attachment_previews_for_message`.
#[derive(Debug, Clone)]
pub struct AttachmentPreview {
    pub part_id: u32,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Parse the IMAP UID out of an `Email.id` produced by the IMAP client.
///
/// `nimbus_imap` formats ids as `"{folder}:{uid}"` — folder names can
/// themselves contain `:` (rare but legal), so we split on the *last*
/// colon. A malformed id yields 0 with a warn log; this can only happen
/// if the upstream id format changes, in which case the cache row will
/// collide on uid=0 and the warning makes it discoverable.
fn uid_from_email_id(id: &str) -> u32 {
    let tail = id.rsplit_once(':').map(|(_, u)| u).unwrap_or(id);
    tail.parse().unwrap_or_else(|_| {
        tracing::warn!("could not parse uid from email id '{id}', defaulting to 0");
        0
    })
}

fn default_cache_path() -> Result<PathBuf, NimbusError> {
    let dir = dirs::config_dir()
        .ok_or_else(|| NimbusError::Storage("cannot determine config directory".into()))?;
    Ok(dir.join("nimbus-mail").join("cache.db"))
}

/// Does this pool-open error look like "wrong key / not a SQLCipher DB"?
///
/// r2d2 wraps the underlying rusqlite error once, and we re-wrap into
/// `CacheError::Open` with the message, so the sentinel strings bubble
/// up in the final `.to_string()`. SQLCipher returns either
/// `SQLITE_NOTADB` ("file is not a database") or `SQLITE_CORRUPT`
/// ("file is encrypted or is not a database") when the key is wrong.
fn is_wrong_key_error(err: &CacheError) -> bool {
    let msg = err.to_string();
    msg.contains("file is not a database") || msg.contains("file is encrypted")
}

/// Delete the cache DB plus its WAL sidecar files (`-wal`, `-shm`).
///
/// Leaving the sidecars behind would let SQLite partially replay the
/// old unencrypted WAL against the new encrypted file on next open.
///
/// Each file is overwritten with random bytes (one pass) and
/// fsync'd before unlink so the encrypted SQLCipher pages don't
/// linger on disk for forensic recovery.  This is fully
/// effective on rotational drives.  On SSDs with wear-levelling
/// the new write may land on a different physical block,
/// leaving the old ciphertext recoverable until the controller
/// reclaims that block — there's no way to force a true secure
/// erase from userspace, so this is best-effort.
fn wipe_cache_files(path: &Path) -> Result<(), CacheError> {
    for suffix in ["", "-wal", "-shm"] {
        let p = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            let mut s = path.as_os_str().to_owned();
            s.push(suffix);
            PathBuf::from(s)
        };
        if p.exists() {
            if let Err(e) = secure_overwrite(&p) {
                // Don't refuse the wipe just because the
                // overwrite step couldn't open the file
                // (read-only filesystem, locked by another
                // process, …) — unlinking the file is still
                // strictly better than leaving it.
                tracing::warn!("secure overwrite of {} failed: {e}", p.display());
            }
            std::fs::remove_file(&p)
                .map_err(|e| CacheError::Open(format!("remove {}: {e}", p.display())))?;
        }
    }
    Ok(())
}

/// Overwrite a file's contents with cryptographic-RNG bytes,
/// flush to disk, before the caller unlinks it.  See
/// `wipe_cache_files` for the threat-model caveats.
fn secure_overwrite(path: &Path) -> Result<(), CacheError> {
    use std::io::Write;
    let len = std::fs::metadata(path)
        .map_err(|e| CacheError::Open(format!("metadata {}: {e}", path.display())))?
        .len();
    if len == 0 {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| CacheError::Open(format!("open-for-overwrite {}: {e}", path.display())))?;
    const CHUNK: usize = 64 * 1024;
    let mut buf = vec![0u8; CHUNK];
    let mut written: u64 = 0;
    while written < len {
        let remaining = len - written;
        let n = (remaining as usize).min(CHUNK);
        getrandom::getrandom(&mut buf[..n])
            .map_err(|e| CacheError::Open(format!("RNG for secure overwrite: {e}")))?;
        f.write_all(&buf[..n])
            .map_err(|e| CacheError::Open(format!("write {}: {e}", path.display())))?;
        written += n as u64;
    }
    f.sync_all()
        .map_err(|e| CacheError::Open(format!("fsync {}: {e}", path.display())))?;
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn make_envelope(uid: u32, folder: &str, offset_min: i64) -> EmailEnvelope {
        EmailEnvelope {
            uid,
            folder: folder.to_string(),
            from: format!("sender-{uid}@example.com"),
            subject: format!("Test subject {uid}"),
            date: Utc::now() - Duration::minutes(offset_min),
            is_read: false,
            is_starred: false,
            account_id: String::new(),
        }
    }

    fn open_test_cache() -> Cache {
        Cache::open_in_memory().expect("open in-memory cache")
    }

    #[test]
    fn migrations_are_idempotent() {
        let cache = open_test_cache();
        // Running again against the same pool should be a no-op.
        let mut conn = cache.conn().expect("checkout");
        schema::run_migrations(&mut conn).expect("second migrate");
    }

    #[test]
    fn upsert_and_read_envelopes_newest_first() {
        let cache = open_test_cache();
        let envs = vec![
            make_envelope(1, "INBOX", 30), // older
            make_envelope(2, "INBOX", 10), // newer
            make_envelope(3, "INBOX", 20),
        ];
        cache
            .upsert_envelopes_for_account("acc-1", &envs)
            .expect("upsert");

        let got = cache.get_envelopes("acc-1", "INBOX", 10).expect("read");
        assert_eq!(got.len(), 3);
        // Newest first: uid 2, then 3, then 1
        assert_eq!(got[0].uid, 2);
        assert_eq!(got[1].uid, 3);
        assert_eq!(got[2].uid, 1);
    }

    #[test]
    fn upsert_refreshes_flags() {
        let cache = open_test_cache();
        let mut env = make_envelope(42, "INBOX", 5);
        cache
            .upsert_envelopes_for_account("acc", std::slice::from_ref(&env))
            .unwrap();

        env.is_read = true;
        env.is_starred = true;
        cache
            .upsert_envelopes_for_account("acc", std::slice::from_ref(&env))
            .unwrap();

        let got = cache.get_envelopes("acc", "INBOX", 5).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].is_read);
        assert!(got[0].is_starred);
    }

    fn make_email(uid: u32, folder: &str) -> Email {
        Email {
            id: format!("{folder}:{uid}"),
            account_id: "acc".to_string(),
            folder: folder.to_string(),
            from: "alice@example.com".into(),
            to: vec!["bob@example.com".into(), "carol@example.com".into()],
            cc: vec!["dave@example.com".into()],
            subject: format!("Hello {uid}"),
            body_text: Some("plain body".into()),
            body_html: Some("<p>html body</p>".into()),
            date: Utc::now(),
            is_read: false,
            is_starred: false,
            has_attachments: true,
            attachments: vec![],
        }
    }

    #[test]
    fn message_roundtrip() {
        let cache = open_test_cache();
        assert!(cache.get_message("acc", "INBOX", 7).unwrap().is_none());

        let email = make_email(7, "INBOX");
        cache.upsert_message(&email).unwrap();

        let got = cache.get_message("acc", "INBOX", 7).unwrap().unwrap();
        assert_eq!(got.subject, "Hello 7");
        assert_eq!(got.body_text.as_deref(), Some("plain body"));
        assert_eq!(got.body_html.as_deref(), Some("<p>html body</p>"));
        assert_eq!(got.to, vec!["bob@example.com", "carol@example.com"]);
        assert_eq!(got.cc, vec!["dave@example.com"]);
        assert!(got.has_attachments);

        // Envelope side is also populated by upsert_message.
        let envs = cache.get_envelopes("acc", "INBOX", 5).unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].uid, 7);
    }

    #[test]
    fn wipe_account_clears_everything() {
        let cache = open_test_cache();
        cache.upsert_message(&make_email(1, "INBOX")).unwrap();

        cache.wipe_account("acc").unwrap();

        assert!(cache.get_envelopes("acc", "INBOX", 5).unwrap().is_empty());
        assert!(cache.get_message("acc", "INBOX", 1).unwrap().is_none());
    }

    #[test]
    fn folders_roundtrip() {
        let cache = open_test_cache();
        let folders = vec![
            Folder {
                name: "INBOX".into(),
                delimiter: Some("/".into()),
                attributes: vec!["\\HasNoChildren".into()],
                unread_count: Some(3),
            },
            Folder {
                name: "Sent".into(),
                delimiter: Some("/".into()),
                attributes: vec!["\\Sent".into(), "\\HasNoChildren".into()],
                unread_count: None,
            },
        ];
        cache.upsert_folders("acc", &folders).unwrap();

        let got = cache.get_folders("acc").unwrap();
        assert_eq!(got.len(), 2);
        // Insertion order is preserved (server's native order).
        assert_eq!(got[0].name, "INBOX");
        assert_eq!(got[0].unread_count, Some(3));
        assert_eq!(got[1].name, "Sent");
        assert_eq!(got[1].attributes, vec!["\\Sent", "\\HasNoChildren"]);

        // Replacing the list wipes the previous rows.
        cache
            .upsert_folders(
                "acc",
                &[Folder {
                    name: "Archive".into(),
                    delimiter: None,
                    attributes: vec![],
                    unread_count: None,
                }],
            )
            .unwrap();
        let got = cache.get_folders("acc").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Archive");
    }

    /// Regression test for #63: `ORDER BY name` would put `Drafts`
    /// ahead of `INBOX` (uppercase 'I' (0x49) sorts before lowercase
    /// 'r' (0x72), but only against same-case neighbours — once mixed
    /// with mixed-case names ASCII order shuffles things). Insertion
    /// order from `upsert_folders` should be preserved verbatim.
    #[test]
    fn folders_preserve_server_order() {
        let cache = open_test_cache();
        let server_order = vec![
            Folder {
                name: "INBOX".into(),
                delimiter: None,
                attributes: vec![],
                unread_count: None,
            },
            Folder {
                name: "Drafts".into(),
                delimiter: None,
                attributes: vec![],
                unread_count: None,
            },
            Folder {
                name: "Sent".into(),
                delimiter: None,
                attributes: vec![],
                unread_count: None,
            },
            Folder {
                name: "Archive".into(),
                delimiter: None,
                attributes: vec![],
                unread_count: None,
            },
            Folder {
                name: "Trash".into(),
                delimiter: None,
                attributes: vec![],
                unread_count: None,
            },
        ];
        cache.upsert_folders("acc", &server_order).unwrap();

        let got: Vec<String> = cache
            .get_folders("acc")
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(got, vec!["INBOX", "Drafts", "Sent", "Archive", "Trash"]);
    }

    #[test]
    fn wipe_folder_is_scoped() {
        let cache = open_test_cache();
        cache.upsert_message(&make_email(1, "INBOX")).unwrap();
        cache.upsert_message(&make_email(2, "Sent")).unwrap();
        cache
            .set_sync_state(
                "acc",
                "INBOX",
                &SyncState {
                    uidvalidity: Some(1),
                    highest_uid_seen: Some(1),
                    last_synced_at: Some(Utc::now()),
                },
            )
            .unwrap();

        cache.wipe_folder("acc", "INBOX").unwrap();

        // INBOX is gone…
        assert!(cache.get_envelopes("acc", "INBOX", 5).unwrap().is_empty());
        assert!(cache.get_message("acc", "INBOX", 1).unwrap().is_none());
        assert!(cache.get_sync_state("acc", "INBOX").unwrap().is_none());
        // …but Sent is untouched.
        assert_eq!(cache.get_envelopes("acc", "Sent", 5).unwrap().len(), 1);
        assert!(cache.get_message("acc", "Sent", 2).unwrap().is_some());
    }

    #[test]
    fn uid_from_email_id_handles_colons_in_folder() {
        assert_eq!(uid_from_email_id("INBOX:42"), 42);
        assert_eq!(uid_from_email_id("Foo:Bar:99"), 99);
        assert_eq!(uid_from_email_id("garbage"), 0);
    }

    #[test]
    fn sync_state_roundtrip() {
        let cache = open_test_cache();
        let now = Utc::now();
        let st = SyncState {
            uidvalidity: Some(1234),
            highest_uid_seen: Some(99),
            last_synced_at: Some(now),
        };
        cache.set_sync_state("acc", "INBOX", &st).unwrap();
        let got = cache.get_sync_state("acc", "INBOX").unwrap().unwrap();
        assert_eq!(got.uidvalidity, Some(1234));
        assert_eq!(got.highest_uid_seen, Some(99));
        // Timestamps round-trip to whole seconds.
        assert_eq!(got.last_synced_at.unwrap().timestamp(), now.timestamp());
    }
}

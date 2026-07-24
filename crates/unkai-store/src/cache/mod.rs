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
pub mod geocode;
pub mod key;
pub mod notes;
pub mod pgp_keys;
pub mod pool;
pub mod schema;
pub mod search;
pub mod smime_certs;
pub mod tasks;

pub use calendars::{
    CachedCalendar, CalendarEventRow, CalendarEventServerHandle, CalendarRow, CalendarSyncState,
    ExpansionInput,
};
pub use contacts::{AddressbookSyncState, ContactRow, ContactServerHandle};
pub use notes::NotesSyncState;
pub use pgp_keys::{PgpKeySource, PgpPublicKeyRow};
pub use search::{DatePeriod, SearchFilters, SearchHit, SearchScope, parse_date_period};
pub use smime_certs::{SmimeCertRow, SmimeCertSource};
pub use tasks::{CachedTaskList, TaskListSyncState};

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, TimeZone, Utc};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{OptionalExtension, params};
use thiserror::Error;
use tracing::{debug, info, warn};
use unkai_core::UnkaiError;
use unkai_core::models::{Email, EmailEnvelope, Folder};

use crate::cache::pool::SqlitePool;

/// One elapsed message reminder (#415) — everything the
/// notification path needs to describe the mail in a toast and
/// deep-link back to it on click.  Produced by
/// [`Cache::due_message_reminders`].
#[derive(Debug, Clone)]
pub struct DueMessageReminder {
    pub account_id: String,
    pub folder: String,
    pub uid: u32,
    pub from: String,
    pub subject: String,
}

/// Receipt-tracking state for one sent mail that asked for a read
/// receipt (RFC 8098, #416).  Produced by
/// [`Cache::get_receipt_status`] and serialised straight over IPC —
/// MailView renders the "receipt requested / read" chip from it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SentReceiptStatus {
    /// Unix-epoch seconds when the mail was sent with the request.
    pub requested_at: i64,
    /// What came back: `"displayed"` / `"deleted"` / …, or `None`
    /// while no receipt has arrived (which may be forever — the
    /// request is advisory and most clients let users decline it).
    pub disposition: Option<String>,
    /// Unix-epoch seconds when the receipt arrived.
    pub disposition_at: Option<i64>,
    /// Address that confirmed, when the report named one.
    pub reporter: Option<String>,
}

/// Errors specific to the cache layer. Converted to `UnkaiError::Storage`
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

impl From<CacheError> for UnkaiError {
    fn from(e: CacheError) -> Self {
        UnkaiError::Storage(e.to_string())
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
    /// `<config-dir>/unkai-mail/cache.db`, and run any pending migrations.
    ///
    /// The DB is encrypted via SQLCipher; the master key is fetched from
    /// (or freshly generated in) the OS keychain. See `key.rs`.
    pub fn open_default() -> Result<Self, UnkaiError> {
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
            Some(hex) => Err(UnkaiError::Storage(format!(
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
    /// Construct a cache handle that is permanently locked — the
    /// pool is `None` and never opens.  Mirrors the state
    /// `open_default` produces in FIDO-only mode before the unlock
    /// IPC runs.  Only for tests that need to exercise
    /// `CacheError::Locked` paths (e.g. unkai-mcp's "vault locked"
    /// rejection) without touching the keychain envelope.
    #[doc(hidden)]
    pub fn locked_for_tests() -> Self {
        Self {
            pool: Arc::new(RwLock::new(None)),
            path: PathBuf::from(":memory:"),
            master_key_hex: Arc::new(RwLock::new(None)),
        }
    }

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
    /// One-shot warm-up: fill in `thread_id` for any rows in this
    /// folder that pre-date the v32 migration or were inserted by a
    /// path that didn't compute it (#334).  Cheap to run on every
    /// read — when every row already has a `thread_id`, the UPDATE
    /// matches zero rows.
    ///
    /// Computes the same canonical key shape as `compute_thread_id`
    /// directly in SQL via COALESCE.  Folder-scoped so a big folder
    /// doesn't tax other folders' read paths.
    pub fn assign_pending_thread_ids(
        &self,
        account_id: &str,
        folder: &str,
    ) -> Result<usize, CacheError> {
        let conn = self.conn()?;
        let updated = conn.execute(
            "UPDATE messages
             SET thread_id = COALESCE(
                 json_extract(references_ids, '$[0]'),
                 message_id,
                 'solo:' || account_id || ':' || folder || ':' || uid
             )
             WHERE account_id = ?1 AND folder = ?2 AND thread_id IS NULL",
            params![account_id, folder],
        )?;
        if updated > 0 {
            tracing::info!(
                "Warmed up thread_id for {updated} envelope(s) in '{account_id}'/'{folder}' (#334)"
            );
        }
        Ok(updated)
    }

    /// Canonical thread identity for the local cache (#334).
    /// Two envelopes share a thread iff they produce the same key:
    ///
    ///   1. `references_ids[0]` — the chain root, when this is a reply.
    ///   2. `message_id` — for top-of-thread originals (future replies
    ///      will carry this value as their first References entry, so
    ///      siblings still resolve correctly).
    ///   3. `solo:<account>:<folder>:<uid>` — synthetic fallback for
    ///      envelopes that have neither (e.g. pre-parser cached rows
    ///      that didn't capture the headers).
    ///
    /// Kept in lock-step with the frontend's old `threadKeyOf` shape
    /// so subjects already bucketed in the UI roll forward to the
    /// same group once they pick up a stored `thread_id`.
    fn compute_thread_id(account_id: &str, env: &EmailEnvelope) -> String {
        if let Some(first) = env.references_ids.first() {
            return first.clone();
        }
        if let Some(mid) = &env.message_id {
            return mid.clone();
        }
        format!("solo:{}:{}:{}", account_id, env.folder, env.uid)
    }

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
            // Note (#255): `replied_kind` is intentionally NOT in
            // the UPDATE clause — IMAP envelope re-fetches don't
            // know which kind of reply happened, so leave whatever
            // we stamped from the send path in place.  `is_answered`
            // *is* refreshed because the IMAP `\Answered` flag is
            // authoritative for "did anyone (incl. another client)
            // answer this".
            // Same reasoning keeps `is_pinned` and
            // `priority_override` (#414) out of this statement
            // entirely: both are local-only user state with no
            // server-side source of truth, so the fetch path must
            // never touch them.  The header-derived `priority` IS
            // written, COALESCE-guarded like the threading headers.
            // `references_ids` is serialised to JSON before the
            // bind so SQLite stores a single TEXT cell (the same
            // shape we use for `attendees_json` etc.).  Empty
            // vector → `NULL` so the indexed column stays sparse
            // for messages that aren't in any thread.
            let mut stmt = tx.prepare(
                "INSERT INTO messages
                   (account_id, folder, uid, from_addr, subject, internal_date,
                    is_read, is_starred, is_answered, cached_at,
                    message_id, in_reply_to, references_ids, thread_id, protection,
                    priority, to_addrs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                         ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT (account_id, folder, uid) DO UPDATE SET
                   from_addr     = excluded.from_addr,
                   subject       = excluded.subject,
                   internal_date = excluded.internal_date,
                   is_read       = excluded.is_read,
                   is_starred    = excluded.is_starred,
                   is_answered   = excluded.is_answered,
                   cached_at     = excluded.cached_at,
                   -- COALESCE so a re-fetch that didn't pick up the
                   -- threading headers (e.g. an old client path) can't
                   -- wipe data we successfully extracted earlier.
                   message_id    = COALESCE(excluded.message_id, messages.message_id),
                   in_reply_to   = COALESCE(excluded.in_reply_to, messages.in_reply_to),
                   references_ids = COALESCE(excluded.references_ids, messages.references_ids),
                   -- #334: `thread_id` is computed from the headers above,
                   -- so a re-fetch that brought new threading info updates
                   -- the row; otherwise we preserve whatever the previous
                   -- write (or the warm-up pass) assigned.
                   thread_id     = COALESCE(excluded.thread_id, messages.thread_id),
                   -- #341 background-decrypt: COALESCE preserves a
                   -- post-decrypt label (\"signed-and-encrypted\") that a
                   -- later envelope re-fetch — which can only see the
                   -- top-level Content-Type — couldn't reproduce.
                   protection    = COALESCE(excluded.protection, messages.protection),
                   -- #414: header-derived priority.  COALESCE so a
                   -- fetch path that didn't parse the priority
                   -- headers can't wipe a value already extracted.
                   priority      = COALESCE(excluded.priority, messages.priority),
                   -- #417: recipient list, COALESCE-guarded like the
                   -- threading headers — a path that didn't capture
                   -- recipients (e.g. a JMAP server omitting `to`)
                   -- can't wipe what an earlier fetch stored.
                   to_addrs      = COALESCE(excluded.to_addrs, messages.to_addrs)",
            )?;
            for env in envelopes {
                let refs_json: Option<String> = if env.references_ids.is_empty() {
                    None
                } else {
                    serde_json::to_string(&env.references_ids).ok()
                };
                // #417: same empty-vector → NULL convention as
                // `references_ids` so the column stays sparse and the
                // COALESCE guard can distinguish "no data" from
                // "captured, zero recipients" — both serialise the
                // same here because a genuinely recipient-less mail
                // (Bcc-only delivery) carries no threading signal
                // anyway.
                let to_json: Option<String> = if env.to_addrs.is_empty() {
                    None
                } else {
                    serde_json::to_string(&env.to_addrs).ok()
                };
                let thread_id = Self::compute_thread_id(account_id, env);
                stmt.execute(params![
                    account_id,
                    env.folder,
                    env.uid as i64,
                    env.from,
                    env.subject,
                    env.date.timestamp(),
                    env.is_read as i64,
                    env.is_starred as i64,
                    env.is_answered as i64,
                    now,
                    env.message_id,
                    env.in_reply_to,
                    refs_json,
                    thread_id,
                    env.protection,
                    env.priority,
                    to_json,
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
        // #334: warm up any pre-migration rows so the read below
        // returns populated thread_id / count fields.  Single
        // UPDATE, no-op when every row is already warm.
        let _ = self.assign_pending_thread_ids(account_id, folder);

        let conn = self.conn()?;
        // Correlated subquery for `thread_total_count` rather than a
        // CTE because we want each row's count computed against the
        // *whole folder*, not just the returned window — a thread
        // whose root is in the newest 50 may have replies older than
        // the window, and the badge needs to report the true total.
        // #341 background-decrypt: COALESCE so the envelope-level
        // protection (stamped at IMAP envelope-fetch time from the
        // Content-Type header) shows up immediately on new mail, while
        // the body-level protection — authoritative once we've decrypted,
        // since it can carry "signed-and-encrypted" which the envelope
        // path can't tell from headers alone — wins whenever the body
        // row exists.
        let mut stmt = conn.prepare(
            "SELECT m.uid, m.folder, m.from_addr, m.subject, m.internal_date,
                    m.is_read, m.is_starred, m.is_answered, m.replied_kind,
                    m.message_id, m.in_reply_to, m.references_ids,
                    m.thread_id,
                    (SELECT COUNT(*) FROM messages m2
                     WHERE m2.account_id = ?1
                       AND m2.folder = ?2
                       AND m2.thread_id = m.thread_id
                       AND m2.pending_action IS NULL) AS thread_total_count,
                    COALESCE(b.protection, m.protection),
                    m.is_pinned, m.priority, m.priority_override,
                    m.reminder_at, m.to_addrs
             FROM messages m
             LEFT JOIN message_bodies b USING (account_id, folder, uid)
             WHERE m.account_id = ?1 AND m.folder = ?2 AND m.pending_action IS NULL
             -- #414: pinned rows first so they can never age out of
             -- the newest-N window; served by
             -- messages_by_folder_pinned_date without a sort step.
             ORDER BY m.is_pinned DESC, m.internal_date DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, folder, limit as i64], |r| {
            let ts: i64 = r.get(4)?;
            let date = Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now);
            let refs_json: Option<String> = r.get(11)?;
            let references_ids: Vec<String> = refs_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let thread_id: Option<String> = r.get(12)?;
            let thread_total_count: Option<i64> = r.get(13)?;
            let protection: Option<String> = r.get(14)?;
            Ok(EmailEnvelope {
                uid: r.get::<_, i64>(0)? as u32,
                folder: r.get(1)?,
                from: r.get(2)?,
                subject: r.get(3)?,
                date,
                is_read: r.get::<_, i64>(5)? != 0,
                is_starred: r.get::<_, i64>(6)? != 0,
                is_answered: r.get::<_, i64>(7)? != 0,
                replied_kind: r.get(8)?,
                account_id: account_id.to_string(),
                message_id: r.get(9)?,
                in_reply_to: r.get(10)?,
                references_ids,
                thread_id,
                thread_total_count: thread_total_count.map(|n| n as u32),
                protection,
                is_pinned: r.get::<_, i64>(15)? != 0,
                priority: r.get(16)?,
                priority_override: r.get(17)?,
                reminder_at: r.get(18)?,
                // #417: recipients, JSON-decoded like references_ids.
                to_addrs: r
                    .get::<_, Option<String>>(19)?
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default(),
                // #416: wire-only marker, never persisted.
                is_mdn_report: false,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Return every cached envelope belonging to a single conversation
    /// (#334).  Folder-scoped because that's the scope the MailList
    /// row lives in, and matches the scope used by the badge count.
    /// Used by the on-expand backfill: the user clicks the chevron on
    /// a thread head and we want every member the local DB knows
    /// about, not just whichever happened to be in the newest-
    /// `PAGE_SIZE` window.  Skips `pending_action` tombstones so a
    /// just-moved message doesn't reappear in the source thread.
    ///
    /// `thread_total_count` is populated via the same correlated
    /// subquery the broader read paths use — i.e. it agrees with
    /// what `get_envelopes` would have returned for any member that
    /// also happens to be in the newest window.  Sorted newest-first.
    pub fn get_envelopes_by_thread(
        &self,
        account_id: &str,
        folder: &str,
        thread_id: &str,
    ) -> Result<Vec<EmailEnvelope>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT m.uid, m.folder, m.from_addr, m.subject, m.internal_date,
                    m.is_read, m.is_starred, m.is_answered, m.replied_kind,
                    m.message_id, m.in_reply_to, m.references_ids,
                    m.thread_id,
                    (SELECT COUNT(*) FROM messages m2
                     WHERE m2.account_id = ?1
                       AND m2.folder = ?2
                       AND m2.thread_id = m.thread_id
                       AND m2.pending_action IS NULL) AS thread_total_count,
                    COALESCE(b.protection, m.protection),
                    m.is_pinned, m.priority, m.priority_override,
                    m.reminder_at, m.to_addrs
             FROM messages m
             LEFT JOIN message_bodies b USING (account_id, folder, uid)
             WHERE m.account_id = ?1
               AND m.folder = ?2
               AND m.thread_id = ?3
               AND m.pending_action IS NULL
             ORDER BY m.internal_date DESC",
        )?;
        let rows = stmt.query_map(params![account_id, folder, thread_id], |r| {
            let ts: i64 = r.get(4)?;
            let date = Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now);
            let refs_json: Option<String> = r.get(11)?;
            let references_ids: Vec<String> = refs_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let thread_id: Option<String> = r.get(12)?;
            let thread_total_count: Option<i64> = r.get(13)?;
            let protection: Option<String> = r.get(14)?;
            Ok(EmailEnvelope {
                uid: r.get::<_, i64>(0)? as u32,
                folder: r.get(1)?,
                from: r.get(2)?,
                subject: r.get(3)?,
                date,
                is_read: r.get::<_, i64>(5)? != 0,
                is_starred: r.get::<_, i64>(6)? != 0,
                is_answered: r.get::<_, i64>(7)? != 0,
                replied_kind: r.get(8)?,
                account_id: account_id.to_string(),
                message_id: r.get(9)?,
                in_reply_to: r.get(10)?,
                references_ids,
                thread_id,
                thread_total_count: thread_total_count.map(|n| n as u32),
                protection,
                is_pinned: r.get::<_, i64>(15)? != 0,
                priority: r.get(16)?,
                priority_override: r.get(17)?,
                reminder_at: r.get(18)?,
                // #417: recipients, JSON-decoded like references_ids.
                to_addrs: r
                    .get::<_, Option<String>>(19)?
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default(),
                // #416: wire-only marker, never persisted.
                is_mdn_report: false,
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
        // #334: warm up every account's copy of this folder before
        // reading.  Cheap — one no-op UPDATE per warm folder.
        if let Ok(mut conn) = self.conn() {
            let tx = conn.transaction().ok();
            if let Some(tx) = tx {
                let _ = tx.execute(
                    "UPDATE messages
                     SET thread_id = COALESCE(
                         json_extract(references_ids, '$[0]'),
                         message_id,
                         'solo:' || account_id || ':' || folder || ':' || uid
                     )
                     WHERE folder = ?1 AND thread_id IS NULL",
                    params![folder],
                );
                let _ = tx.commit();
            }
        }

        let conn = self.conn()?;
        // Subquery scopes the count by `(account_id, folder, thread_id)`
        // because a thread_id can collide across accounts (two unrelated
        // users may end up with the same `<msg-id@host>` if a sender
        // delivers to both inboxes) — counting across accounts would
        // wrongly merge those.
        let mut stmt = conn.prepare(
            "SELECT m.account_id, m.uid, m.folder, m.from_addr, m.subject, m.internal_date,
                    m.is_read, m.is_starred, m.is_answered, m.replied_kind,
                    m.message_id, m.in_reply_to, m.references_ids,
                    m.thread_id,
                    (SELECT COUNT(*) FROM messages m2
                     WHERE m2.account_id = m.account_id
                       AND m2.folder = m.folder
                       AND m2.thread_id = m.thread_id
                       AND m2.pending_action IS NULL) AS thread_total_count,
                    COALESCE(b.protection, m.protection),
                    m.is_pinned, m.priority, m.priority_override,
                    m.reminder_at, m.to_addrs
             FROM messages m
             LEFT JOIN message_bodies b USING (account_id, folder, uid)
             WHERE m.folder = ?1 AND m.pending_action IS NULL
             ORDER BY m.is_pinned DESC, m.internal_date DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![folder, limit as i64], |r| {
            let ts: i64 = r.get(5)?;
            let date = Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now);
            let refs_json: Option<String> = r.get(12)?;
            let references_ids: Vec<String> = refs_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let thread_id: Option<String> = r.get(13)?;
            let thread_total_count: Option<i64> = r.get(14)?;
            let protection: Option<String> = r.get(15)?;
            Ok(EmailEnvelope {
                account_id: r.get(0)?,
                uid: r.get::<_, i64>(1)? as u32,
                folder: r.get(2)?,
                from: r.get(3)?,
                subject: r.get(4)?,
                date,
                is_read: r.get::<_, i64>(6)? != 0,
                is_starred: r.get::<_, i64>(7)? != 0,
                is_answered: r.get::<_, i64>(8)? != 0,
                replied_kind: r.get(9)?,
                message_id: r.get(10)?,
                in_reply_to: r.get(11)?,
                references_ids,
                thread_id,
                thread_total_count: thread_total_count.map(|n| n as u32),
                protection,
                is_pinned: r.get::<_, i64>(16)? != 0,
                priority: r.get(17)?,
                priority_override: r.get(18)?,
                reminder_at: r.get(19)?,
                // #417: recipients, JSON-decoded like references_ids.
                to_addrs: r
                    .get::<_, Option<String>>(20)?
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default(),
                // #416: wire-only marker, never persisted.
                is_mdn_report: false,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Return the newest `limit` envelopes across the given
    /// `(account_id, folder)` pairs, newest-first. Powers the global
    /// "All Sent" / "All Drafts" views, where each account stores
    /// outgoing mail in a differently-named folder (`Sent`, `Sent
    /// Items`, `Gesendete Elemente`, `[Gmail]/Sent Mail`, …) so the
    /// single-folder-name query used by `get_unified_envelopes` can't
    /// match them all at once.
    ///
    /// Empty `pairs` short-circuits to `Ok(vec![])` so callers don't
    /// have to special-case "no accounts have a Sent folder yet".
    pub fn get_unified_envelopes_by_pairs(
        &self,
        pairs: &[(String, String)],
        limit: u32,
    ) -> Result<Vec<EmailEnvelope>, CacheError> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn()?;
        // Build `(account_id = ?N AND folder = ?N+1) OR …` so each
        // account's resolved special-use folder contributes only the
        // rows that actually live in it. Parameterised — no string
        // interpolation of folder names into the SQL.
        let mut where_clause = String::new();
        for i in 0..pairs.len() {
            if i > 0 {
                where_clause.push_str(" OR ");
            }
            let a = 2 * i + 1;
            let b = 2 * i + 2;
            where_clause.push_str(&format!("(account_id = ?{a} AND folder = ?{b})"));
        }
        let limit_param = 2 * pairs.len() + 1;
        // #334: thread_total_count is folder-and-account-scoped, same
        // shape as the single-account read.
        let sql = format!(
            "SELECT m.account_id, m.uid, m.folder, m.from_addr, m.subject, m.internal_date,
                    m.is_read, m.is_starred, m.is_answered, m.replied_kind,
                    m.message_id, m.in_reply_to, m.references_ids,
                    m.thread_id,
                    (SELECT COUNT(*) FROM messages m2
                     WHERE m2.account_id = m.account_id
                       AND m2.folder = m.folder
                       AND m2.thread_id = m.thread_id
                       AND m2.pending_action IS NULL) AS thread_total_count,
                    COALESCE(b.protection, m.protection),
                    m.is_pinned, m.priority, m.priority_override,
                    m.reminder_at, m.to_addrs
             FROM messages m
             LEFT JOIN message_bodies b USING (account_id, folder, uid)
             WHERE ({where_clause}) AND m.pending_action IS NULL
             ORDER BY m.is_pinned DESC, m.internal_date DESC
             LIMIT ?{limit_param}"
        );
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(2 * pairs.len() + 1);
        for (account_id, folder) in pairs {
            params_vec.push(account_id);
            params_vec.push(folder);
        }
        let limit_i64 = limit as i64;
        params_vec.push(&limit_i64);

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_vec.as_slice(), |r| {
            let ts: i64 = r.get(5)?;
            let date = Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now);
            let refs_json: Option<String> = r.get(12)?;
            let references_ids: Vec<String> = refs_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let thread_id: Option<String> = r.get(13)?;
            let thread_total_count: Option<i64> = r.get(14)?;
            let protection: Option<String> = r.get(15)?;
            Ok(EmailEnvelope {
                account_id: r.get(0)?,
                uid: r.get::<_, i64>(1)? as u32,
                folder: r.get(2)?,
                from: r.get(3)?,
                subject: r.get(4)?,
                date,
                is_read: r.get::<_, i64>(6)? != 0,
                is_starred: r.get::<_, i64>(7)? != 0,
                is_answered: r.get::<_, i64>(8)? != 0,
                replied_kind: r.get(9)?,
                message_id: r.get(10)?,
                in_reply_to: r.get(11)?,
                references_ids,
                thread_id,
                thread_total_count: thread_total_count.map(|n| n as u32),
                protection,
                is_pinned: r.get::<_, i64>(16)? != 0,
                priority: r.get(17)?,
                priority_override: r.get(18)?,
                reminder_at: r.get(19)?,
                // #417: recipients, JSON-decoded like references_ids.
                to_addrs: r
                    .get::<_, Option<String>>(20)?
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default(),
                // #416: wire-only marker, never persisted.
                is_mdn_report: false,
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

    /// Newest `limit` cached envelope UIDs for a folder, sorted by
    /// `internal_date DESC` — the same ordering the mail list
    /// renders.  Used by the flag-refresh path (#255 follow-up) to
    /// catch up on cross-client `\Seen` / `\Flagged` / `\Answered`
    /// changes on the visible window without re-pulling the whole
    /// folder's flag set.
    pub fn list_recent_envelope_uids(
        &self,
        account_id: &str,
        folder: &str,
        limit: u32,
    ) -> Result<Vec<u32>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT uid FROM messages
             WHERE account_id = ?1 AND folder = ?2
             ORDER BY internal_date DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, folder, limit as i64], |r| {
            r.get::<_, i64>(0)
        })?;
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

    /// Same as [`mark_message_pending`] but creates a placeholder
    /// row when the target UID isn't cached yet (#292 follow-up).
    ///
    /// The minimize-save path APPENDs to IMAP without writing the
    /// new envelope into the cache — `save_draft` only invokes
    /// `remove_envelope` on the replaced source, never `upsert` on
    /// the freshly-APPENDed copy.  A subsequent send that targets
    /// the minimize-saved UID hits a tombstone-via-UPDATE that
    /// silently misses (zero rows updated), so the next
    /// `poll_folder` re-fetches the row from IMAP and INSERTs it
    /// without a `pending_action`, causing the row to flash back
    /// into the visible list until the real IMAP DELETE catches
    /// up.
    ///
    /// This variant guarantees the tombstone lands: UPDATE first,
    /// and if no row matched, INSERT a placeholder carrying the
    /// pending flag.  `get_envelopes` filters out any row with
    /// `pending_action IS NOT NULL`, so the placeholder is
    /// invisible.  When `poll_folder`'s upsert later writes the
    /// real envelope, the `ON CONFLICT … DO UPDATE` clause
    /// deliberately *doesn't* touch `pending_action`, so the
    /// tombstone survives the merge.  On IMAP success the row
    /// gets dropped entirely via `remove_envelope`; on IMAP
    /// failure `clear_message_pending` un-tombstones, which
    /// briefly exposes the placeholder's empty fields — accepted
    /// as a rare edge case since the next poll fills them in.
    pub fn upsert_message_pending(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        action: &str,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let updated = conn.execute(
            "UPDATE messages SET pending_action = ?4
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64, action],
        )?;
        if updated > 0 {
            return Ok(());
        }
        // Row missing — insert a placeholder.  `INSERT OR IGNORE`
        // covers the race where a concurrent poll inserts the
        // real row between our UPDATE-found-nothing and this
        // INSERT; in that case we'd need a second UPDATE to flip
        // pending_action on, so we re-run the UPDATE
        // unconditionally below.
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT OR IGNORE INTO messages
                (account_id, folder, uid, internal_date, cached_at, pending_action)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![account_id, folder, uid as i64, now, now, action],
        )?;
        conn.execute(
            "UPDATE messages SET pending_action = ?4
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3
                   AND pending_action IS NULL",
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

    /// Reconcile the cached `is_read` / `is_starred` / `is_answered`
    /// flags for a batch of envelope rows against fresh server-side
    /// values (#255 follow-up).
    ///
    /// The standard envelope-fetch path is incremental — it never
    /// re-reads flags on UIDs older than the bookmark.  Without
    /// this catch-up, marks made from another mail client (read on
    /// a phone, answered from webmail, starred elsewhere) never
    /// round-trip into the local cache, and Unkai's mail list
    /// drifts out of sync with reality.
    ///
    /// `replied_kind` is intentionally not touched — it's
    /// Unkai-only metadata about *how* the user replied via
    /// Compose, and the IMAP `\Answered` bit doesn't carry the
    /// kind, so leave whatever the send path stamped earlier.
    /// The `unread_count` on the folder is kept in sync by
    /// crediting / debiting per-row read-state flips inside the
    /// same transaction.
    ///
    /// Each tuple is `(uid, is_read, is_starred, is_answered)`.
    /// UIDs that don't exist in the cache are silently skipped —
    /// they got expunged between fetch and reconcile, and the
    /// envelope-fetch path will sweep them out on its own
    /// reconcile pass.
    pub fn reconcile_envelope_flags(
        &self,
        account_id: &str,
        folder: &str,
        snapshots: &[(u32, bool, bool, bool)],
    ) -> Result<u32, CacheError> {
        if snapshots.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let mut unread_delta: i64 = 0;
        let mut changed: u32 = 0;
        for (uid, is_read, is_starred, is_answered) in snapshots {
            // Fetch current flags so we know whether to bump the
            // folder's unread badge.  `query_row` returns
            // `QueryReturnedNoRows` when the UID isn't cached —
            // skip those.
            let prior: Option<(bool, bool, bool)> = tx
                .query_row(
                    "SELECT is_read, is_starred, is_answered FROM messages
                     WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
                    params![account_id, folder, *uid as i64],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)? != 0,
                            r.get::<_, i64>(1)? != 0,
                            r.get::<_, i64>(2)? != 0,
                        ))
                    },
                )
                .ok();
            let Some((was_read, was_starred, was_answered)) = prior else {
                continue;
            };
            if was_read == *is_read && was_starred == *is_starred && was_answered == *is_answered {
                continue;
            }

            tx.execute(
                "UPDATE messages
                 SET is_read = ?4, is_starred = ?5, is_answered = ?6
                 WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
                params![
                    account_id,
                    folder,
                    *uid as i64,
                    *is_read as i64,
                    *is_starred as i64,
                    *is_answered as i64,
                ],
            )?;
            changed += 1;

            // Bump the unread accumulator: if the flag flipped
            // unread → read, the folder count drops by one; if it
            // flipped read → unread, it goes up by one.
            if was_read != *is_read {
                unread_delta += if *is_read { -1 } else { 1 };
            }
        }

        if unread_delta != 0 {
            tx.execute(
                "UPDATE folders
                 SET unread_count = MAX(COALESCE(unread_count, 0) + ?3, 0)
                 WHERE account_id = ?1 AND name = ?2",
                params![account_id, folder, unread_delta],
            )?;
        }

        tx.commit()?;
        Ok(changed)
    }

    /// Stamp a cached envelope as answered (#255).  Sets both
    /// `is_answered = 1` (so the row keeps the generic-reply
    /// fallback even after the IMAP `\Answered` flag round-trips
    /// from the server) and `replied_kind` to one of `"reply"`,
    /// `"reply-all"`, or `"meeting"`.
    ///
    /// Called from the Compose send path after a successful
    /// reply / reply-all / meeting reply.  Idempotent: re-running
    /// with the same kind is a no-op; re-running with a different
    /// kind overwrites — useful only in unusual flows where the
    /// user replies, then later "responds with meeting" on the
    /// same source thread.
    pub fn mark_envelope_replied(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        kind: &str,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE messages
             SET is_answered = 1,
                 replied_kind = ?4
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64, kind],
        )?;
        Ok(())
    }

    /// Set or clear the cached `is_starred` flag (#414) — the local
    /// half of the user's flag toggle.  The caller propagates the
    /// same change to the server (IMAP `\Flagged` / JMAP `$flagged`)
    /// after this optimistic write, mirroring the
    /// `mark_envelope_read` pattern.  No folder-counter bookkeeping
    /// is needed here — unlike read state, flags don't feed a badge.
    pub fn mark_envelope_starred(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        starred: bool,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE messages SET is_starred = ?4
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64, starred as i64],
        )?;
        Ok(())
    }

    /// Set or clear the local-only pin state (#414).  Nothing to
    /// propagate — pins have no server-side equivalent, so this
    /// write IS the whole operation.
    pub fn mark_envelope_pinned(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        pinned: bool,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE messages SET is_pinned = ?4
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64, pinned as i64],
        )?;
        Ok(())
    }

    /// Set or clear the user's priority override (#414).  `priority`
    /// is `"high"` / `"normal"` / `"low"`, or `None` to drop the
    /// override entirely (display falls back to the header-derived
    /// `priority` column).  Local-only, like the pin.
    pub fn set_envelope_priority(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        priority: Option<&str>,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE messages SET priority_override = ?4
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64, priority],
        )?;
        Ok(())
    }

    /// Set or clear the per-message reminder (#415).  `remind_at` is
    /// unix-epoch seconds at which the reminder should fire, or
    /// `None` to drop a pending reminder.  Local-only like the pin —
    /// there is nothing to propagate, so this write IS the whole
    /// operation.  The background scanner also calls this with
    /// `None` to mark a reminder as fired.
    pub fn set_message_reminder(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        remind_at: Option<i64>,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE messages SET reminder_at = ?4
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64, remind_at],
        )?;
        Ok(())
    }

    /// Every message whose reminder time has elapsed (#415):
    /// `reminder_at <= now`, tombstoned rows excluded.  No lower
    /// bound on purpose — a reminder that elapsed while the app was
    /// closed should fire (late) on the first scan after launch
    /// rather than be silently dropped; that is the whole
    /// restart-survival contract.  Served by the partial
    /// `messages_by_reminder` index, so a mailbox with no pending
    /// reminders answers this from an empty index.
    pub fn due_message_reminders(&self, now: i64) -> Result<Vec<DueMessageReminder>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT account_id, folder, uid, from_addr, subject
             FROM messages
             WHERE reminder_at IS NOT NULL
               AND reminder_at <= ?1
               AND pending_action IS NULL
             ORDER BY reminder_at ASC",
        )?;
        let rows = stmt.query_map(params![now], |r| {
            Ok(DueMessageReminder {
                account_id: r.get(0)?,
                folder: r.get(1)?,
                uid: r.get::<_, i64>(2)? as u32,
                from: r.get(3)?,
                subject: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Read the local-only organizational state for one cached
    /// envelope (#414, #415, #416): `(is_pinned, priority_override,
    /// reminder_at, mdn_handled)`.  Used by the full-message fetch
    /// paths to overlay pin / override / reminder / receipt-handled
    /// onto an `Email` that came off the wire — the protocols can't
    /// know local state, and without the overlay a fresh network
    /// fetch would render the message unpinned in the UI even
    /// though the cache row still says otherwise.  `Ok(None)` when
    /// the UID isn't cached (nothing local to overlay).
    #[allow(clippy::type_complexity)]
    pub fn envelope_local_state(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<Option<(bool, Option<String>, Option<i64>, Option<String>)>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT is_pinned, priority_override, reminder_at, mdn_handled FROM messages
                 WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
                params![account_id, folder, uid as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? != 0,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    // ── Read receipts / MDN (#416) ─────────────────────────────

    /// Record how the user (or their policy) resolved an incoming
    /// read-receipt request: `"sent"` or `"declined"`.  Local-only
    /// like the pin — this is what keeps the reading pane from
    /// asking about the same message twice.
    pub fn set_mdn_handled(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        handled: &str,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE messages SET mdn_handled = ?4
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
            params![account_id, folder, uid as i64, handled],
        )?;
        Ok(())
    }

    /// Track that a just-sent mail asked for a read receipt (#416).
    /// Keyed on the sent Message-ID (bracket-free, #277 convention)
    /// — the only identifier both the sent copy and a future
    /// `message/disposition-notification` reply share.  Idempotent:
    /// a retry of the same send updates the timestamp rather than
    /// erroring.
    pub fn record_receipt_request(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO sent_receipts (account_id, message_id, requested_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (account_id, message_id) DO UPDATE SET
                requested_at = excluded.requested_at",
            params![account_id, message_id, Utc::now().timestamp()],
        )?;
        Ok(())
    }

    /// Does this account have any receipt request still waiting for
    /// an answer?  Cheap gate for the sync path: incoming
    /// `multipart/report` bodies are only worth fetching while at
    /// least one request could match, so accounts that never ask
    /// for receipts never pay the extra fetches.
    pub fn has_pending_receipt_requests(&self, account_id: &str) -> Result<bool, CacheError> {
        let conn = self.conn()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sent_receipts
             WHERE account_id = ?1 AND disposition IS NULL",
            params![account_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Patch a receipt request with what an incoming
    /// `message/disposition-notification` reported (#416).  Returns
    /// `true` when a tracked request matched the report's
    /// `Original-Message-ID`; `false` means the report referenced a
    /// mail we never tracked (sent from another client, or before
    /// this feature) and nothing was recorded.
    pub fn record_receipt_disposition(
        &self,
        account_id: &str,
        original_message_id: &str,
        disposition: &str,
        reporter: Option<&str>,
    ) -> Result<bool, CacheError> {
        let conn = self.conn()?;
        let updated = conn.execute(
            "UPDATE sent_receipts
             SET disposition = ?3, disposition_at = ?4, reporter = ?5
             WHERE account_id = ?1 AND message_id = ?2",
            params![
                account_id,
                original_message_id,
                disposition,
                Utc::now().timestamp(),
                reporter,
            ],
        )?;
        Ok(updated > 0)
    }

    /// Receipt-tracking state for one sent mail, by Message-ID
    /// (#416).  `Ok(None)` = this mail never asked for a receipt
    /// (or was sent before the feature / from another client), so
    /// MailView renders no chip at all.
    pub fn get_receipt_status(
        &self,
        account_id: &str,
        message_id: &str,
    ) -> Result<Option<SentReceiptStatus>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT requested_at, disposition, disposition_at, reporter
                 FROM sent_receipts
                 WHERE account_id = ?1 AND message_id = ?2",
                params![account_id, message_id],
                |r| {
                    Ok(SentReceiptStatus {
                        requested_at: r.get(0)?,
                        disposition: r.get(1)?,
                        disposition_at: r.get(2)?,
                        reporter: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
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
    /// Feeds the tray tooltip ("Unkai Mail — 3 unread") and any
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

        // #417: recipient list for the envelope row — empty → NULL
        // (same convention as the envelope-batch upsert) so the
        // COALESCE guard below can tell "no data" from "captured".
        let env_to_json: Option<String> = if email.to.is_empty() {
            None
        } else {
            serde_json::to_string(&email.to).ok()
        };

        // Envelope row — mirrors upsert_envelopes_for_account but inside
        // the same transaction as the body so the two can't drift.
        tx.execute(
            "INSERT INTO messages
                (account_id, folder, uid, from_addr, subject, internal_date,
                 is_read, is_starred, cached_at, priority, mdn_requested_to,
                 to_addrs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (account_id, folder, uid) DO UPDATE SET
                from_addr     = excluded.from_addr,
                subject       = excluded.subject,
                internal_date = excluded.internal_date,
                is_read       = excluded.is_read,
                is_starred    = excluded.is_starred,
                cached_at     = excluded.cached_at,
                -- #414: full-message fetches parse the priority
                -- headers too; COALESCE-guarded like the envelope
                -- upsert so a path that didn't can't wipe the value.
                priority      = COALESCE(excluded.priority, messages.priority),
                -- #416: receipt request, header-derived like priority.
                -- (`mdn_handled` is local-only and deliberately absent
                -- from this statement, like is_pinned / reminder_at.)
                mdn_requested_to = COALESCE(excluded.mdn_requested_to, messages.mdn_requested_to),
                -- #417: a full-message fetch always knows the real
                -- recipient list, so opening a message back-fills the
                -- participant data pre-migration envelope rows lack.
                to_addrs      = COALESCE(excluded.to_addrs, messages.to_addrs)",
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
                email.priority,
                email.mdn_requested_to,
                env_to_json,
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
                 attachments, protection, signature_status, signer_fingerprint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT (account_id, folder, uid) DO UPDATE SET
                body_text          = excluded.body_text,
                body_html          = excluded.body_html,
                has_attachments    = excluded.has_attachments,
                raw_size           = excluded.raw_size,
                cached_at          = excluded.cached_at,
                to_addrs           = excluded.to_addrs,
                cc_addrs           = excluded.cc_addrs,
                attachments        = excluded.attachments,
                protection         = excluded.protection,
                signature_status   = excluded.signature_status,
                signer_fingerprint = excluded.signer_fingerprint",
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
                email.protection,
                email.signature_status,
                email.signer_fingerprint,
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
                        b.to_addrs, b.cc_addrs, b.attachments,
                        m.message_id, m.in_reply_to, m.references_ids,
                        b.protection, b.signature_status, b.signer_fingerprint,
                        m.is_pinned, m.priority, m.priority_override,
                        m.reminder_at, m.mdn_requested_to, m.mdn_handled
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
                    let refs_json: Option<String> = r.get(13)?;
                    let references_ids: Vec<String> = refs_json
                        .as_deref()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_default();
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
                        message_id: r.get(11)?,
                        in_reply_to: r.get(12)?,
                        references_ids,
                        protection: r.get(14)?,
                        signature_status: r.get(15)?,
                        signer_fingerprint: r.get(16)?,
                        is_pinned: r.get::<_, i64>(17)? != 0,
                        priority: r.get(18)?,
                        priority_override: r.get(19)?,
                        reminder_at: r.get(20)?,
                        mdn_requested_to: r.get(21)?,
                        mdn_handled: r.get(22)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    // ── Encrypted ciphertext cache (#341) ───────────────────────
    //
    // Before this cache, every Decrypt click and every encrypted
    // attachment download issued a fresh `UID FETCH BODY.PEEK[]`
    // round-trip to IMAP — measurable latency on slow networks and
    // a hard fail offline.  Storing the raw `.eml` bytes alongside
    // the (decrypted) body lets the next decrypt for the same UID
    // run entirely from local storage.

    /// Cache the raw RFC 5322 bytes of an encrypted message so
    /// later decrypts / attachment downloads for the same UID can
    /// skip IMAP.
    ///
    /// UPSERT — creates a `message_bodies` row stamped with just
    /// this column when one doesn't already exist (background-
    /// decrypt during sync runs *before* the user has ever opened
    /// the message, so the row may not be there yet), or patches
    /// the column in place when it does.  The envelope row in
    /// `messages` must already exist — the FK keeps us honest, and
    /// every caller has already gone through envelope-fetch first.
    pub fn put_encrypted_raw_eml(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        raw: &[u8],
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO message_bodies
                 (account_id, folder, uid, cached_at, encrypted_raw_eml)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (account_id, folder, uid) DO UPDATE SET
                 encrypted_raw_eml = excluded.encrypted_raw_eml",
            params![account_id, folder, uid as i64, now, raw],
        )?;
        Ok(())
    }

    /// Load the cached raw `.eml` bytes for an encrypted message,
    /// if any.  See [`Cache::put_encrypted_raw_eml`] for the write
    /// side.
    ///
    /// `Ok(None)` covers: never-cached UIDs, plaintext messages
    /// (we don't cache those), and pre-#341 rows.  Callers treat
    /// any of those as a cache miss and fall through to IMAP.
    pub fn get_encrypted_raw_eml(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<Option<Vec<u8>>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT encrypted_raw_eml
                   FROM message_bodies
                  WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
                params![account_id, folder, uid as i64],
                |r| r.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .flatten();
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

    // ── Outbox (#276) ────────────────────────────────────────────
    //
    // Local-only "send queue" — every send routes through this
    // table first, the SMTP send runs in a spawned drain task.
    // Rows that drain successfully are removed within the same
    // tick (sub-second on a healthy network); rows that fail
    // stay queued and the periodic background-sync drain retries
    // them.  Surfaces in the UI as a synthetic "Outbox" folder
    // that only appears in the sidebar when the queue is
    // non-empty.

    /// Push a fresh outgoing message onto the queue.  Returns the
    /// generated row id so the caller (`send_email`) can hand it
    /// to the spawned drain task.
    /// Atomic claim of a queued outbox row for the duration of a
    /// single drain attempt (#292 follow-up).
    ///
    /// Both [`send_email`]'s spawned drain task and the periodic
    /// `drain_outbox_sweep` operate on the same `outbox_messages`
    /// table, and `try_drain_outbox_entry` was previously a plain
    /// read-then-act with no exclusion.  If those two paths
    /// happened to overlap (e.g. the user clicked Send a few
    /// seconds before a sweep tick), both would read the row,
    /// both would push the message through SMTP + APPEND-to-Sent,
    /// and the recipient would receive the same mail twice.
    ///
    /// This is a CAS: bump `last_attempt_at` to `now` only when
    /// no other drain has touched the row inside the last
    /// `claim_ttl_secs` seconds.  Returns `true` when the caller
    /// won the claim and should proceed, `false` when another
    /// drain holds the row.  The TTL means that even if a drain
    /// task panics or the process dies mid-send the row isn't
    /// permanently stuck — the next sweep past the TTL boundary
    /// reclaims it.
    pub fn claim_outbox_for_drain(&self, id: i64, claim_ttl_secs: i64) -> Result<bool, CacheError> {
        let conn = self.conn()?;
        let now = Utc::now().timestamp();
        let threshold = now - claim_ttl_secs;
        let updated = conn.execute(
            "UPDATE outbox_messages
             SET last_attempt_at = ?1
             WHERE id = ?2
               AND (last_attempt_at IS NULL OR last_attempt_at < ?3)",
            params![now, id, threshold],
        )?;
        Ok(updated > 0)
    }

    /// Unconditional variant of [`claim_outbox_for_drain`] — stamps
    /// `last_attempt_at` to `now` regardless of any prior claim
    /// inside the TTL window.  Used by the user-driven retry path
    /// (#341 / PR #361) where the CAS-style guard the periodic
    /// sweep and post-enqueue spawn rely on is the wrong shape:
    /// the user clicked Retry on a row whose previous attempt
    /// already failed (otherwise the row would be gone), so there
    /// is no concurrent drain to protect against, and refusing the
    /// re-claim inside the 30 s window made the awaiting retry
    /// command return `Ok` without actually running — a silent
    /// success that closed the passphrase panel deceptively.
    /// Returns `true` when the row exists, `false` when it has
    /// vanished (drained, deleted) in between — same shape as
    /// `claim_outbox_for_drain` so callers can branch identically.
    pub fn force_claim_outbox_for_drain(&self, id: i64) -> Result<bool, CacheError> {
        let conn = self.conn()?;
        let now = Utc::now().timestamp();
        let updated = conn.execute(
            "UPDATE outbox_messages SET last_attempt_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(updated > 0)
    }

    pub fn enqueue_outbox(&self, input: &OutboxEnqueue) -> Result<i64, CacheError> {
        let conn = self.conn()?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO outbox_messages
               (account_id, outgoing_json, replied_to_json,
                from_header, to_display, subject,
                queued_at, attempt_count, last_attempt_at, last_error,
                skip_sent_copy)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, NULL, NULL, ?8)",
            params![
                input.account_id,
                input.outgoing_json,
                input.replied_to_json,
                input.from_header,
                input.to_display,
                input.subject,
                now,
                input.skip_sent_copy as i64,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Snapshot of one queued message.  Returned by `list_outbox`
    /// (per-account, for the UI list view) and by
    /// `take_pending_outbox_drain` (for the retry sweep).
    pub fn list_outbox(&self, account_id: &str) -> Result<Vec<OutboxRow>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, outgoing_json, replied_to_json,
                    from_header, to_display, subject,
                    queued_at, attempt_count, last_attempt_at, last_error,
                    skip_sent_copy
             FROM outbox_messages
             WHERE account_id = ?1
             ORDER BY queued_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id], outbox_row_from_sql)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Every queued row across every account, oldest-first so the
    /// drain sweep retries in the order the user submitted them.
    /// Used by the `background_sync_loop` retry pass.
    pub fn list_all_outbox(&self) -> Result<Vec<OutboxRow>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, outgoing_json, replied_to_json,
                    from_header, to_display, subject,
                    queued_at, attempt_count, last_attempt_at, last_error,
                    skip_sent_copy
             FROM outbox_messages
             ORDER BY queued_at ASC",
        )?;
        let rows = stmt.query_map(params![], outbox_row_from_sql)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Fetch one queued row by id — used by the drain task (when
    /// the caller knows the row it just enqueued) and by the
    /// `edit_outbox_entry` Tauri command.
    pub fn get_outbox(&self, id: i64) -> Result<Option<OutboxRow>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT id, account_id, outgoing_json, replied_to_json,
                        from_header, to_display, subject,
                        queued_at, attempt_count, last_attempt_at, last_error,
                        skip_sent_copy
                 FROM outbox_messages
                 WHERE id = ?1",
                params![id],
                outbox_row_from_sql,
            )
            .optional()?;
        Ok(row)
    }

    /// Bookkeeping after a drain attempt failed: bump the count,
    /// stamp the last attempt time, store the human-readable
    /// error.  Doesn't delete — the row stays for the next sweep.
    pub fn record_outbox_failure(&self, id: i64, error: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let now = Utc::now().timestamp();
        conn.execute(
            "UPDATE outbox_messages
             SET attempt_count = attempt_count + 1,
                 last_attempt_at = ?2,
                 last_error = ?3
             WHERE id = ?1",
            params![id, now, error],
        )?;
        Ok(())
    }

    /// Drop a queued row.  Called by the drain task on success
    /// (the SMTP send went through) and by the
    /// `delete_outbox_entry` Tauri command (user dismissed it).
    pub fn remove_outbox(&self, id: i64) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM outbox_messages WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Total queued rows across every account.  Cheap aggregate
    /// for the badge count on the synthetic "Outbox" sidebar
    /// folder.
    pub fn count_outbox(&self) -> Result<u32, CacheError> {
        let conn = self.conn()?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM outbox_messages", params![], |r| {
            r.get(0)
        })?;
        Ok(n.max(0) as u32)
    }

    /// Per-account count for the per-account sidebar badge.
    pub fn count_outbox_for_account(&self, account_id: &str) -> Result<u32, CacheError> {
        let conn = self.conn()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM outbox_messages WHERE account_id = ?1",
            params![account_id],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u32)
    }

    /// Queued-row counts grouped by account (#290).  Drives the
    /// per-account decision of "render the synthetic Outbox folder
    /// in this sidebar?" — the previous global count caused the
    /// folder to leak into every account's sidebar whenever any
    /// account had a pending send.  Accounts with zero queued
    /// rows are omitted so the caller can `?? 0` without an
    /// "Outbox (0)" badge showing up.
    pub fn count_outbox_by_account(
        &self,
    ) -> Result<std::collections::HashMap<String, u32>, CacheError> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare("SELECT account_id, COUNT(*) FROM outbox_messages GROUP BY account_id")?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let n: i64 = r.get(1)?;
            Ok((id, n.max(0) as u32))
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
}

/// Row shape inserted into `outbox_messages` by
/// `Cache::enqueue_outbox`.  Mirrors the columns of the table —
/// kept as a separate struct so callers don't have to hand-build
/// a `params![]` tuple at every enqueue site.
#[derive(Debug, Clone)]
pub struct OutboxEnqueue {
    pub account_id: String,
    pub outgoing_json: String,
    pub replied_to_json: Option<String>,
    pub from_header: String,
    pub to_display: String,
    pub subject: String,
    pub skip_sent_copy: bool,
}

/// One queued message read out of `outbox_messages`.  Returned by
/// the `list_outbox` / `get_outbox` / `list_all_outbox` paths.
#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub id: i64,
    pub account_id: String,
    pub outgoing_json: String,
    pub replied_to_json: Option<String>,
    pub from_header: String,
    pub to_display: String,
    pub subject: String,
    pub queued_at: i64,
    pub attempt_count: u32,
    pub last_attempt_at: Option<i64>,
    pub last_error: Option<String>,
    pub skip_sent_copy: bool,
}

/// Shared row-mapping helper used by every outbox SELECT.  The
/// column order must match the `SELECT` clauses in
/// `list_outbox` / `list_all_outbox` / `get_outbox`.
fn outbox_row_from_sql(r: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxRow> {
    Ok(OutboxRow {
        id: r.get(0)?,
        account_id: r.get(1)?,
        outgoing_json: r.get(2)?,
        replied_to_json: r.get(3)?,
        from_header: r.get(4)?,
        to_display: r.get(5)?,
        subject: r.get(6)?,
        queued_at: r.get(7)?,
        attempt_count: r.get::<_, i64>(8)?.max(0) as u32,
        last_attempt_at: r.get(9)?,
        last_error: r.get(10)?,
        skip_sent_copy: r.get::<_, i64>(11)? != 0,
    })
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
/// `unkai_imap` formats ids as `"{folder}:{uid}"` — folder names can
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

fn default_cache_path() -> Result<PathBuf, UnkaiError> {
    let dir = dirs::config_dir()
        .ok_or_else(|| UnkaiError::Storage("cannot determine config directory".into()))?;
    Ok(dir.join("unkai-mail").join("cache.db"))
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
        getrandom::fill(&mut buf[..n])
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
            to_addrs: vec![format!("Jane Smith <recipient-{uid}@example.com>")],
            subject: format!("Test subject {uid}"),
            date: Utc::now() - Duration::minutes(offset_min),
            is_read: false,
            is_starred: false,
            is_answered: false,
            replied_kind: None,
            account_id: String::new(),
            message_id: None,
            in_reply_to: None,
            references_ids: Vec::new(),
            thread_id: None,
            thread_total_count: None,
            protection: None,
            is_pinned: false,
            reminder_at: None,
            priority: None,
            priority_override: None,
            is_mdn_report: false,
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

    /// #417: the `To:` recipient list round-trips through the
    /// envelope upsert, and the COALESCE guard keeps an earlier
    /// capture alive when a later fetch path produced no recipients
    /// (empty vector → NULL bind).
    #[test]
    fn to_addrs_roundtrip_and_coalesce_guard() {
        let cache = open_test_cache();
        let mut env = make_envelope(7, "INBOX", 5);
        env.to_addrs = vec![
            "Alex Morgan <alex@example.com>".to_string(),
            "team@example.com".to_string(),
        ];
        cache
            .upsert_envelopes_for_account("acc", std::slice::from_ref(&env))
            .unwrap();

        let got = cache.get_envelopes("acc", "INBOX", 5).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].to_addrs, env.to_addrs);

        // Re-fetch that didn't capture recipients: must not wipe them.
        env.to_addrs = Vec::new();
        cache
            .upsert_envelopes_for_account("acc", std::slice::from_ref(&env))
            .unwrap();
        let got = cache.get_envelopes("acc", "INBOX", 5).unwrap();
        assert_eq!(
            got[0].to_addrs,
            vec![
                "Alex Morgan <alex@example.com>".to_string(),
                "team@example.com".to_string(),
            ],
            "empty re-fetch must not clobber stored recipients",
        );
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

    /// #414: pinned envelopes sort above everything regardless of
    /// date, and the local-only pin / priority-override state
    /// survives an envelope re-fetch (which must never write those
    /// columns).  The header-derived `priority` IS refreshed, but
    /// COALESCE-guarded so a fetch that produced `None` can't wipe
    /// an earlier value.
    #[test]
    fn pin_and_priority_roundtrip() {
        let cache = open_test_cache();
        let mut old_env = make_envelope(1, "INBOX", 60); // older
        old_env.priority = Some("high".into());
        let envs = vec![old_env, make_envelope(2, "INBOX", 5)];
        cache.upsert_envelopes_for_account("acc", &envs).unwrap();

        // Pin the *older* message and override its priority.
        cache.mark_envelope_pinned("acc", "INBOX", 1, true).unwrap();
        cache
            .set_envelope_priority("acc", "INBOX", 1, Some("normal"))
            .unwrap();

        let got = cache.get_envelopes("acc", "INBOX", 10).unwrap();
        assert_eq!(got[0].uid, 1, "pinned row must sort first");
        assert!(got[0].is_pinned);
        assert_eq!(got[0].priority.as_deref(), Some("high"));
        assert_eq!(got[0].priority_override.as_deref(), Some("normal"));

        // A re-fetch without priority headers must clobber neither
        // the pin, the override, nor the earlier header priority.
        cache.upsert_envelopes_for_account("acc", &envs).unwrap();
        let got = cache.get_envelopes("acc", "INBOX", 10).unwrap();
        assert_eq!(got[0].uid, 1);
        assert!(got[0].is_pinned);
        assert_eq!(got[0].priority.as_deref(), Some("high"));
        assert_eq!(got[0].priority_override.as_deref(), Some("normal"));

        // Unpin + clear override → back to date order, no override.
        cache
            .mark_envelope_pinned("acc", "INBOX", 1, false)
            .unwrap();
        cache
            .set_envelope_priority("acc", "INBOX", 1, None)
            .unwrap();
        let got = cache.get_envelopes("acc", "INBOX", 10).unwrap();
        assert_eq!(got[0].uid, 2);
        assert!(!got[1].is_pinned);
        assert!(got[1].priority_override.is_none());
    }

    /// #414: the flag setter flips `is_starred` in place.
    #[test]
    fn starred_setter_roundtrip() {
        let cache = open_test_cache();
        let envs = vec![make_envelope(9, "INBOX", 1)];
        cache.upsert_envelopes_for_account("acc", &envs).unwrap();

        cache
            .mark_envelope_starred("acc", "INBOX", 9, true)
            .unwrap();
        let got = cache.get_envelopes("acc", "INBOX", 5).unwrap();
        assert!(got[0].is_starred);

        cache
            .mark_envelope_starred("acc", "INBOX", 9, false)
            .unwrap();
        let got = cache.get_envelopes("acc", "INBOX", 5).unwrap();
        assert!(!got[0].is_starred);
    }

    /// #415: the reminder round-trip.  Setting a reminder surfaces
    /// it on the envelope read, `due_message_reminders` only
    /// returns rows whose time has elapsed, an envelope re-fetch
    /// can't clobber the pending reminder, and clearing (what the
    /// scanner does after firing) empties the due list.
    #[test]
    fn reminder_roundtrip() {
        let cache = open_test_cache();
        let envs = vec![make_envelope(1, "INBOX", 60), make_envelope(2, "INBOX", 5)];
        cache.upsert_envelopes_for_account("acc", &envs).unwrap();

        cache
            .set_message_reminder("acc", "INBOX", 1, Some(1_000))
            .unwrap();

        let got = cache.get_envelopes("acc", "INBOX", 10).unwrap();
        let row = got.iter().find(|e| e.uid == 1).unwrap();
        assert_eq!(row.reminder_at, Some(1_000));

        // Not yet due …
        assert!(cache.due_message_reminders(999).unwrap().is_empty());
        // … due exactly at / after the stored moment, carrying the
        // fields the notification needs.
        let due = cache.due_message_reminders(1_000).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].uid, 1);
        assert_eq!(due[0].account_id, "acc");
        assert_eq!(due[0].folder, "INBOX");

        // A background envelope re-fetch must leave the pending
        // reminder alone (local-only column, like the pin).
        cache.upsert_envelopes_for_account("acc", &envs).unwrap();
        let got = cache.get_envelopes("acc", "INBOX", 10).unwrap();
        let row = got.iter().find(|e| e.uid == 1).unwrap();
        assert_eq!(row.reminder_at, Some(1_000));

        // Clearing (fired, or user removed it) empties the due list.
        cache.set_message_reminder("acc", "INBOX", 1, None).unwrap();
        assert!(cache.due_message_reminders(2_000).unwrap().is_empty());
        let got = cache.get_envelopes("acc", "INBOX", 10).unwrap();
        assert!(got.iter().all(|e| e.reminder_at.is_none()));
    }

    #[test]
    fn sent_receipt_lifecycle() {
        // #416: request → pending → disposition recorded → status.
        let cache = open_test_cache();

        assert!(!cache.has_pending_receipt_requests("acc").unwrap());
        cache.record_receipt_request("acc", "mid-1@host").unwrap();
        assert!(cache.has_pending_receipt_requests("acc").unwrap());

        let status = cache
            .get_receipt_status("acc", "mid-1@host")
            .unwrap()
            .expect("row must exist after request");
        assert!(status.disposition.is_none());

        // A report for an untracked Message-ID records nothing.
        assert!(
            !cache
                .record_receipt_disposition("acc", "unknown@host", "displayed", None)
                .unwrap()
        );

        // The matching report patches the row and clears the
        // pending gate.
        assert!(
            cache
                .record_receipt_disposition(
                    "acc",
                    "mid-1@host",
                    "displayed",
                    Some("reader@example.org"),
                )
                .unwrap()
        );
        assert!(!cache.has_pending_receipt_requests("acc").unwrap());
        let status = cache
            .get_receipt_status("acc", "mid-1@host")
            .unwrap()
            .expect("row must survive the patch");
        assert_eq!(status.disposition.as_deref(), Some("displayed"));
        assert_eq!(status.reporter.as_deref(), Some("reader@example.org"));
        assert!(status.disposition_at.is_some());

        // Other accounts never see acc's rows.
        assert!(
            cache
                .get_receipt_status("other", "mid-1@host")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mdn_request_persists_and_handled_state_survives_refetch() {
        // #416: the header-derived request lands via upsert_message;
        // the local-only handled flag survives envelope re-fetches.
        let cache = open_test_cache();
        let mut email = make_email(1, "INBOX");
        email.mdn_requested_to = Some("sender@example.org".into());
        cache.upsert_message(&email).unwrap();

        let got = cache.get_message("acc", "INBOX", 1).unwrap().unwrap();
        assert_eq!(got.mdn_requested_to.as_deref(), Some("sender@example.org"));
        assert_eq!(got.mdn_handled, None);

        cache.set_mdn_handled("acc", "INBOX", 1, "sent").unwrap();
        let (.., mdn_handled) = cache
            .envelope_local_state("acc", "INBOX", 1)
            .unwrap()
            .expect("cached row");
        assert_eq!(mdn_handled.as_deref(), Some("sent"));

        // An envelope re-fetch (which knows nothing about MDN state)
        // must clobber neither the request nor the handled flag.
        let env = make_envelope(1, "INBOX", 0);
        cache.upsert_envelopes_for_account("acc", &[env]).unwrap();
        let got = cache.get_message("acc", "INBOX", 1).unwrap().unwrap();
        assert_eq!(got.mdn_requested_to.as_deref(), Some("sender@example.org"));
        assert_eq!(got.mdn_handled.as_deref(), Some("sent"));
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
            message_id: None,
            in_reply_to: None,
            references_ids: Vec::new(),
            protection: None,
            signature_status: None,
            signer_fingerprint: None,
            is_pinned: false,
            reminder_at: None,
            priority: None,
            priority_override: None,
            mdn_requested_to: None,
            mdn_handled: None,
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
    fn outbox_enqueue_list_count_remove_roundtrip() {
        let cache = open_test_cache();
        assert_eq!(cache.count_outbox().unwrap(), 0);
        assert!(cache.list_outbox("acc-a").unwrap().is_empty());

        let id = cache
            .enqueue_outbox(&OutboxEnqueue {
                account_id: "acc-a".into(),
                outgoing_json: r#"{"from":"a@x","to":["b@x"],"cc":[],"bcc":[],"reply_to":null,"subject":"hi","body_text":"hello","body_html":null,"attachments":[]}"#.into(),
                replied_to_json: None,
                from_header: "a@x".into(),
                to_display: "b@x".into(),
                subject: "hi".into(),
                skip_sent_copy: false,
            })
            .unwrap();
        assert!(id > 0);
        assert_eq!(cache.count_outbox().unwrap(), 1);
        assert_eq!(cache.count_outbox_for_account("acc-a").unwrap(), 1);
        assert_eq!(cache.count_outbox_for_account("acc-other").unwrap(), 0);
        // Per-account map: acc-a present with 1, acc-other omitted.
        let by_acc = cache.count_outbox_by_account().unwrap();
        assert_eq!(by_acc.get("acc-a").copied(), Some(1));
        assert!(!by_acc.contains_key("acc-other"));

        let rows = cache.list_outbox("acc-a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject, "hi");
        assert_eq!(rows[0].attempt_count, 0);
        assert!(rows[0].last_error.is_none());

        // Failure bookkeeping bumps the count + records the error,
        // but the row stays for the next sweep.
        cache
            .record_outbox_failure(id, "Connection refused")
            .unwrap();
        let rows = cache.list_outbox("acc-a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attempt_count, 1);
        assert_eq!(rows[0].last_error.as_deref(), Some("Connection refused"));
        assert!(rows[0].last_attempt_at.is_some());

        // get_outbox returns the same shape by id.
        let one = cache.get_outbox(id).unwrap().unwrap();
        assert_eq!(one.id, id);
        assert_eq!(one.subject, "hi");

        // Removal drops the row + clears the count.
        cache.remove_outbox(id).unwrap();
        assert_eq!(cache.count_outbox().unwrap(), 0);
        assert!(cache.list_outbox("acc-a").unwrap().is_empty());
        assert!(cache.get_outbox(id).unwrap().is_none());
        assert!(cache.count_outbox_by_account().unwrap().is_empty());
    }

    #[test]
    fn outbox_list_all_orders_oldest_first() {
        let cache = open_test_cache();
        let mk = |account: &str, subj: &str| OutboxEnqueue {
            account_id: account.into(),
            outgoing_json: "{}".into(),
            replied_to_json: None,
            from_header: "x@x".into(),
            to_display: "y@y".into(),
            subject: subj.into(),
            skip_sent_copy: false,
        };
        let a = cache.enqueue_outbox(&mk("acc-a", "first")).unwrap();
        // Sleep one second so the queued_at timestamps differ —
        // SQLite's strftime resolution is per-second.
        std::thread::sleep(std::time::Duration::from_secs(1));
        let b = cache.enqueue_outbox(&mk("acc-b", "second")).unwrap();

        let all = cache.list_all_outbox().unwrap();
        assert_eq!(all.len(), 2);
        // The drain sweep relies on oldest-first so the user's
        // earlier sends ship before later ones.
        assert_eq!(all[0].id, a);
        assert_eq!(all[0].subject, "first");
        assert_eq!(all[1].id, b);
        assert_eq!(all[1].subject, "second");
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

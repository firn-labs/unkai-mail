//! Machine-level shared cache (#532).
//!
//! The one storage surface that deliberately sits *outside* the
//! profile dimension (#530).  Profiles isolate everything personal —
//! but a handful of tables are pure machine-level caches of public
//! data, and duplicating those per profile only multiplies downloads
//! and disk. Those live here, in a single plaintext SQLite file at
//! `<config_dir>/unkai-mail/shared.db` ([`crate::ProfilePaths::shared_db`]),
//! shared by every profile the process hosts.
//!
//! # Scope ownership — the whole point of this split
//!
//! After #532 the rule is binary, with nothing ambiguous left:
//!
//! - [`crate::Cache`] (`profiles/<id>/cache.db`, SQLCipher) =
//!   **profile-scoped data.** Everything account-keyed, plus the
//!   account-less tables that still describe the profile's own
//!   correspondence and choices: `rsvp_responses`,
//!   `cancelled_invites`, `mailing_list_settings`, `pgp_public_keys`,
//!   `smime_certs` — and `geocode_cache`, which *could* be shared
//!   mechanically but stays per-profile on purpose: the queries are
//!   the locations the user typed, i.e. personal correspondence
//!   context that belongs under the profile's own encryption key.
//! - [`SharedCache`] (this module, plaintext) = **machine-scoped
//!   data.** Today that is exactly the URLhaus snapshot
//!   (`urlhaus_urls` + `urlhaus_meta`): a verbatim hourly copy of
//!   abuse.ch's public feed, identical for every profile, carrying
//!   no user data at all. One copy, one hourly download per machine
//!   (#165's refresh worker runs once per process since #532).
//!
//! When a future migration adds an account-less table, classify it
//! against this rule explicitly and put it in the right file.
//!
//! # Why plaintext
//!
//! `cache.db` is SQLCipher-encrypted because it holds mail. This file
//! holds a public dataset — encrypting it would force an awkward
//! "machine-level key" concept (no profile owns the file, so no
//! `master-key:<profile-id>` entry fits) for zero confidentiality
//! gain. Integrity is not a concern either beyond what file
//! permissions give us: the feed is advisory UI signal, not a
//! security boundary (see `link_check`'s module docs).
//!
//! # Migrations
//!
//! Same append-only convention as `cache::schema` — an ordered list
//! of SQL blocks, a `schema_version` table, only ever push new
//! entries. The ladder is separate from the profile cache's because
//! the two files version independently.

use std::path::Path;

use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use tracing::info;
use unkai_core::UnkaiError;

use crate::cache::CacheError;
use crate::cache::pool::{self, SqlitePool};

/// Ordered migration scripts for `shared.db`.  Index `i` migrates
/// version `i` → `i+1`.  **Only ever append** — see `cache::schema`'s
/// module docs for the full rules; they apply here unchanged.
const MIGRATIONS: &[&str] = &[
    // ─────────────────────────────────────────────────────────────
    // v0 → v1: the URLhaus snapshot (#165), moved out of the
    // per-profile cache by #532.  Same DDL the profile cache used
    // (its copy is dropped by profile-cache migration v46) — the
    // `link_check` store module is the only reader/writer.
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE urlhaus_urls (
        url            TEXT PRIMARY KEY,
        host           TEXT NOT NULL,
        threat         TEXT NOT NULL DEFAULT '',
        tags           TEXT NOT NULL DEFAULT '',  -- comma-separated tag list
        date_added     INTEGER NOT NULL,           -- unix epoch seconds
        last_refreshed INTEGER NOT NULL            -- unix epoch seconds
    );

    CREATE INDEX urlhaus_by_host ON urlhaus_urls (host);

    CREATE TABLE urlhaus_meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
];

const SCHEMA_VERSION_SQL: &str = r#"
    CREATE TABLE IF NOT EXISTS schema_version (
        id      INTEGER PRIMARY KEY CHECK (id = 1),
        version INTEGER NOT NULL
    );
    INSERT OR IGNORE INTO schema_version (id, version) VALUES (1, 0);
"#;

/// Bring `shared.db` up to the latest version — same
/// transaction-per-migration shape as `cache::schema::run_migrations`.
fn run_migrations(conn: &mut Connection) -> Result<(), CacheError> {
    conn.execute_batch(SCHEMA_VERSION_SQL)
        .map_err(|e| CacheError::Migration(format!("shared: init schema_version: {e}")))?;

    let current: i64 = conn
        .query_row("SELECT version FROM schema_version WHERE id = 1", [], |r| {
            r.get(0)
        })
        .map_err(|e| CacheError::Migration(format!("shared: read schema_version: {e}")))?;

    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn
            .transaction()
            .map_err(|e| CacheError::Migration(format!("shared: begin tx: {e}")))?;
        tx.execute_batch(sql).map_err(|e| {
            CacheError::Migration(format!("shared migration v{} → v{}: {e}", i, i + 1))
        })?;
        tx.execute(
            "UPDATE schema_version SET version = ?1 WHERE id = 1",
            [(i + 1) as i64],
        )
        .map_err(|e| CacheError::Migration(format!("shared: bump version: {e}")))?;
        tx.commit()
            .map_err(|e| CacheError::Migration(format!("shared: commit tx: {e}")))?;
    }
    Ok(())
}

/// Handle to the machine-level shared cache — **machine-scoped data
/// only** (today: the URLhaus feed snapshot; see the module docs for
/// the profile-vs-machine classification rule).  Cheap to clone,
/// like [`crate::Cache`]: one pool inside an `Arc`, one instance per
/// process, the same clone handed to every profile context.
///
/// Never locked: unlike the profile cache there is no FIDO gate here
/// (nothing personal to protect), so link-safety lookups keep working
/// even while a profile's own cache sits locked behind the unlock
/// screen.
#[derive(Clone)]
pub struct SharedCache {
    pool: SqlitePool,
}

impl SharedCache {
    /// Open (or create) `shared.db` at `path`
    /// ([`crate::ProfilePaths::shared_db`]) and run any pending
    /// migrations.  Plain SQLite — see the module docs for why no
    /// SQLCipher key is involved.
    pub fn open(path: &Path) -> Result<Self, UnkaiError> {
        info!("Opening machine-level shared cache at {}", path.display());
        let pool = pool::open_plain_pool(path)?;
        let mut conn = pool.get().map_err(CacheError::from)?;
        run_migrations(&mut conn)?;
        drop(conn);
        Ok(Self { pool })
    }

    /// Fresh in-memory instance for tests — mirrors
    /// [`crate::Cache::open_in_memory`].
    pub fn open_in_memory() -> Result<Self, CacheError> {
        let pool = pool::open_plain_memory_pool()?;
        let mut conn = pool.get()?;
        run_migrations(&mut conn)?;
        drop(conn);
        Ok(Self { pool })
    }

    /// Borrow a pooled connection.  `pub(crate)` so the store
    /// modules for machine-scoped tables (`link_check`) reach the
    /// DB the same way `Cache::conn` serves the profile modules.
    pub(crate) fn conn(&self) -> Result<PooledConnection<SqliteConnectionManager>, CacheError> {
        Ok(self.pool.get()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_create_the_urlhaus_tables() {
        let shared = SharedCache::open_in_memory().expect("open shared cache");
        let conn = shared.conn().expect("conn");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('urlhaus_urls', 'urlhaus_meta')",
                [],
                |r| r.get(0),
            )
            .expect("count tables");
        assert_eq!(n, 2, "both urlhaus tables must exist after migration");
    }

    #[test]
    fn reopening_is_idempotent() {
        let dir =
            std::env::temp_dir().join(format!("unkai-shared-cache-test-{}", std::process::id()));
        let path = dir.join("shared.db");
        let _ = std::fs::remove_dir_all(&dir);

        let first = SharedCache::open(&path).expect("first open");
        drop(first);
        // Second open finds schema_version already at the target and
        // must not error trying to re-create existing tables.
        let second = SharedCache::open(&path).expect("second open");
        let v: i64 = second
            .conn()
            .expect("conn")
            .query_row("SELECT version FROM schema_version WHERE id = 1", [], |r| {
                r.get(0)
            })
            .expect("read version");
        assert_eq!(v, MIGRATIONS.len() as i64);

        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

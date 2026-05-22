//! Connection pool and PRAGMA tuning for the local mail cache.
//!
//! SQLite is a single-file embedded database. "Scaling" it means choosing
//! sane PRAGMAs and serialising writes; the library choice matters less
//! than those. The settings below are the standard high-throughput
//! desktop-app recipe:
//!
//! - **`PRAGMA key`** — SQLCipher's unlock pragma. MUST be the first
//!   statement on every connection, before any query touches the DB.
//!   The key is a 32-byte raw binary value, passed as `x'<hex>'` so
//!   SQLCipher skips PBKDF2 derivation (we already have CSPRNG bytes).
//!
//! - **WAL (Write-Ahead Logging)** — readers never block writers and vice
//!   versa. Without WAL, every write takes an exclusive lock on the whole
//!   DB, which would freeze the UI while the sync thread writes envelopes.
//!
//! - **`synchronous = NORMAL`** — WAL is checkpointed at shutdown rather
//!   than on every commit. Safe (we still survive crashes, just not a
//!   hard OS-level power loss mid-commit) and dramatically faster.
//!
//! - **`busy_timeout = 5000ms`** — if a second connection finds the write
//!   lock held, wait up to 5s instead of failing immediately. This papers
//!   over brief contention between the sync thread and a UI query.
//!
//! - **`foreign_keys = ON`** — SQLite ignores FK constraints by default;
//!   we want `ON DELETE CASCADE` on `message_bodies` to actually fire.
//!
//! # Why r2d2
//!
//! A pool lets N reader connections run concurrently against a WAL
//! database — useful once we start doing searches or background sync
//! while the UI is also pulling envelopes. One pool, shared by the
//! whole app, created once at startup.

use std::path::Path;
use std::time::Duration;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

use crate::cache::CacheError;

pub type SqlitePool = Pool<SqliteConnectionManager>;

/// Apply `PRAGMA key` first (SQLCipher unlock), then the scaling-oriented
/// PRAGMAs. r2d2 calls this on every newly opened pooled connection via
/// `.with_init(...)`, so each connection is fully unlocked and configured
/// before it leaves the pool.
///
/// `key_hex` MUST be a 64-character lowercase hex string; see `key.rs`.
/// We validate the length here because a malformed literal would otherwise
/// be silently treated by SQLCipher as a passphrase and derive a different
/// (wrong) key, locking us out of our own DB.
fn apply_pragmas(conn: &mut Connection, key_hex: &str) -> rusqlite::Result<()> {
    if key_hex.len() != 64 || !key_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(rusqlite::Error::InvalidParameterName(
            "DB master key must be 64 hex chars".into(),
        ));
    }
    // `PRAGMA key` does not accept bound parameters — it has to be a
    // literal. The key_hex check above keeps this free of SQL injection.
    conn.execute_batch(&format!("PRAGMA key = \"x'{key_hex}'\";"))?;

    // Touch the DB so SQLCipher actually tries to decrypt a page. Without
    // a real read, a wrong key doesn't surface until the first real query
    // — and we'd rather the error happen here, inside open_pool.
    let _: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;

    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    Ok(())
}

/// Open (or create) the cache database at `path` and return a ready-to-use pool.
///
/// The `key_hex` is the 64-char hex master key (see `key::get_or_create_master_key`).
/// Every connection the pool hands out will already be unlocked.
pub fn open_pool(path: &Path, key_hex: String) -> Result<SqlitePool, CacheError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CacheError::Open(format!("create cache dir: {e}")))?;
    }

    let manager =
        SqliteConnectionManager::file(path).with_init(move |c| apply_pragmas(c, &key_hex));

    // Small pool — a desktop app rarely benefits from more than a handful
    // of connections. The cost of a connection is ~100KB of SQLite state.
    Pool::builder()
        .max_size(8)
        .build(manager)
        .map_err(|e| CacheError::Open(format!("build pool: {e}")))
}

/// Convenience: open an in-memory pool with a unique, process-local name.
///
/// Each call gets a fresh memory DB that only its own pool can see — this
/// isolates tests that run in parallel. The `file::<name>:?mode=memory&
/// cache=shared` URI lets multiple pooled connections share one DB while
/// the counter-driven name prevents other tests from crashing into it.
///
/// `pub(crate)` and *not* `cfg(test)` so sibling modules
/// (e.g. `account_store`) can build a Cache for their own unit tests.
/// Production code should never call this — it bypasses the keychain.
pub(crate) fn open_memory_pool() -> Result<SqlitePool, CacheError> {
    use rusqlite::OpenFlags;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let uri = format!("file:unkai_test_mem_{id}?mode=memory&cache=shared");
    // `SQLITE_OPEN_URI` is required for the URI form to be honoured.
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_URI;
    // Fixed all-zero test key — tests never touch the real keychain,
    // and the on-disk risk is nil since it's a memory DB.
    let key_hex = "0".repeat(64);
    let manager = SqliteConnectionManager::file(uri)
        .with_flags(flags)
        .with_init(move |c| apply_pragmas(c, &key_hex));
    Pool::builder()
        .max_size(4)
        .build(manager)
        .map_err(|e| CacheError::Open(format!("build memory pool: {e}")))
}

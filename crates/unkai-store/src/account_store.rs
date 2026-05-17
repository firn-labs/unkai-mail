//! Persistent account storage backed by the SQLCipher cache.
//!
//! Accounts live in the encrypted cache (issue #60) so the whole
//! account record (hosts, signatures, TLS-trust state) is encrypted
//! at rest under the same key as the message bodies.

use rusqlite::params;
use tracing::{debug, info};
use unkai_core::UnkaiError;
use unkai_core::models::Account;

use crate::Cache;

/// Load all saved accounts. Returns an empty list on a fresh
/// install.
pub fn load_accounts(cache: &Cache) -> Result<Vec<Account>, UnkaiError> {
    read_all(cache)
}

/// Read every row out of the cache, ordered by insertion. Used by
/// both `load_accounts` and the dedup check in `add_account`.
fn read_all(cache: &Cache) -> Result<Vec<Account>, UnkaiError> {
    let conn = conn(cache)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, display_name, email, imap_host, imap_port,
                    smtp_host, smtp_port, use_jmap, jmap_url, signature,
                    folder_icons_json, trusted_certs_json,
                    folder_icon_overrides_json,
                    emoji, sort_order, person_name
             FROM accounts
             ORDER BY rowid",
        )
        .map_err(|e| UnkaiError::Storage(format!("prepare load_accounts: {e}")))?;

    let rows = stmt
        .query_map([], row_to_account)
        .map_err(|e| UnkaiError::Storage(format!("query load_accounts: {e}")))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| UnkaiError::Storage(format!("row load_accounts: {e}")))?);
    }
    debug!("Loaded {} account(s) from cache", out.len());
    Ok(out)
}

fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    let folder_icons_json: String = r.get(10)?;
    let folder_icons = serde_json::from_str(&folder_icons_json).unwrap_or_default();
    let trusted_certs_json: String = r.get(11)?;
    let trusted_certs = serde_json::from_str(&trusted_certs_json).unwrap_or_default();
    let folder_icon_overrides_json: String = r.get(12)?;
    let folder_icon_overrides =
        serde_json::from_str(&folder_icon_overrides_json).unwrap_or_default();
    Ok(Account {
        id: r.get(0)?,
        display_name: r.get(1)?,
        email: r.get(2)?,
        imap_host: r.get(3)?,
        imap_port: r.get::<_, i64>(4)? as u16,
        smtp_host: r.get(5)?,
        smtp_port: r.get::<_, i64>(6)? as u16,
        use_jmap: r.get::<_, i64>(7)? != 0,
        jmap_url: r.get(8)?,
        signature: r.get(9)?,
        folder_icons,
        trusted_certs,
        folder_icon_overrides,
        emoji: r.get(13)?,
        sort_order: r.get::<_, i64>(14)? as i32,
        person_name: r.get(15)?,
    })
}

/// Add a new account. Errors if an account with this id already exists
/// — same contract the JSON-backed implementation had, so callers don't
/// need to change their handling.
pub fn add_account(cache: &Cache, account: Account) -> Result<(), UnkaiError> {
    if read_all(cache)?.iter().any(|a| a.id == account.id) {
        return Err(UnkaiError::Storage(format!(
            "account with id '{}' already exists",
            account.id
        )));
    }

    info!(
        "Adding account '{}' ({})",
        account.display_name, account.email
    );
    insert_one(cache, &account)
}

fn insert_one(cache: &Cache, account: &Account) -> Result<(), UnkaiError> {
    let folder_icons_json = serde_json::to_string(&account.folder_icons)
        .map_err(|e| UnkaiError::Storage(format!("serialize folder_icons: {e}")))?;
    let trusted_certs_json = serde_json::to_string(&account.trusted_certs)
        .map_err(|e| UnkaiError::Storage(format!("serialize trusted_certs: {e}")))?;
    let folder_icon_overrides_json = serde_json::to_string(&account.folder_icon_overrides)
        .map_err(|e| UnkaiError::Storage(format!("serialize folder_icon_overrides: {e}")))?;
    let conn = conn(cache)?;
    conn.execute(
        "INSERT INTO accounts
            (id, display_name, email, imap_host, imap_port,
             smtp_host, smtp_port, use_jmap, jmap_url, signature,
             folder_icons_json, trusted_certs_json,
             folder_icon_overrides_json, created_at,
             emoji, sort_order, person_name)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17)",
        params![
            account.id,
            account.display_name,
            account.email,
            account.imap_host,
            account.imap_port as i64,
            account.smtp_host,
            account.smtp_port as i64,
            account.use_jmap as i64,
            account.jmap_url,
            account.signature,
            folder_icons_json,
            trusted_certs_json,
            folder_icon_overrides_json,
            chrono::Utc::now().timestamp(),
            account.emoji,
            account.sort_order as i64,
            account.person_name,
        ],
    )
    .map_err(|e| UnkaiError::Storage(format!("insert account: {e}")))?;
    Ok(())
}

/// Remove an account by id. Returns an error if the id wasn't found.
pub fn remove_account(cache: &Cache, id: &str) -> Result<(), UnkaiError> {
    let conn = conn(cache)?;
    let removed = conn
        .execute("DELETE FROM accounts WHERE id = ?1", params![id])
        .map_err(|e| UnkaiError::Storage(format!("delete account: {e}")))?;

    if removed == 0 {
        return Err(UnkaiError::Storage(format!(
            "no account found with id '{id}'"
        )));
    }
    info!("Removed account '{id}'");
    Ok(())
}

/// Update an existing account in place. The id must already exist —
/// matches the JSON-backed contract so callers don't have to branch.
pub fn update_account(cache: &Cache, updated: Account) -> Result<(), UnkaiError> {
    let folder_icons_json = serde_json::to_string(&updated.folder_icons)
        .map_err(|e| UnkaiError::Storage(format!("serialize folder_icons: {e}")))?;
    let trusted_certs_json = serde_json::to_string(&updated.trusted_certs)
        .map_err(|e| UnkaiError::Storage(format!("serialize trusted_certs: {e}")))?;
    let folder_icon_overrides_json = serde_json::to_string(&updated.folder_icon_overrides)
        .map_err(|e| UnkaiError::Storage(format!("serialize folder_icon_overrides: {e}")))?;
    let conn = conn(cache)?;
    let changed = conn
        .execute(
            "UPDATE accounts
             SET display_name               = ?2,
                 email                      = ?3,
                 imap_host                  = ?4,
                 imap_port                  = ?5,
                 smtp_host                  = ?6,
                 smtp_port                  = ?7,
                 use_jmap                   = ?8,
                 jmap_url                   = ?9,
                 signature                  = ?10,
                 folder_icons_json          = ?11,
                 trusted_certs_json         = ?12,
                 folder_icon_overrides_json = ?13,
                 emoji                      = ?14,
                 sort_order                 = ?15,
                 person_name                = ?16
             WHERE id = ?1",
            params![
                updated.id,
                updated.display_name,
                updated.email,
                updated.imap_host,
                updated.imap_port as i64,
                updated.smtp_host,
                updated.smtp_port as i64,
                updated.use_jmap as i64,
                updated.jmap_url,
                updated.signature,
                folder_icons_json,
                trusted_certs_json,
                folder_icon_overrides_json,
                updated.emoji,
                updated.sort_order as i64,
                updated.person_name,
            ],
        )
        .map_err(|e| UnkaiError::Storage(format!("update account: {e}")))?;

    if changed == 0 {
        return Err(UnkaiError::Storage(format!(
            "no account found with id '{}'",
            updated.id
        )));
    }
    info!(
        "Updated account '{}' ({})",
        updated.display_name, updated.email
    );
    Ok(())
}

/// Borrow a pooled connection.  Thin wrapper around
/// `Cache::conn` that maps the cache-error variant into
/// `UnkaiError` so callers can use `?` cleanly.  Returns
/// `UnkaiError::Auth` (rather than Storage) for the locked
/// case so the IPC layer can route it to the unlock UI.
fn conn(
    cache: &Cache,
) -> Result<r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager>, UnkaiError> {
    cache.conn().map_err(|e| match e {
        crate::cache::CacheError::Locked => {
            UnkaiError::Auth("Cache is locked — authenticate to unlock".into())
        }
        other => UnkaiError::Storage(format!("checkout cache conn: {other}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_cache() -> Cache {
        Cache::open_in_memory().expect("open in-memory cache")
    }

    fn test_account(id: &str) -> Account {
        Account {
            id: id.to_string(),
            display_name: "Test User".into(),
            email: "test@example.com".into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            use_jmap: false,
            jmap_url: None,
            signature: None,
            folder_icons: Vec::new(),
            trusted_certs: Vec::new(),
            folder_icon_overrides: Default::default(),
            emoji: None,
            sort_order: 0,
            person_name: None,
        }
    }

    #[test]
    fn add_load_remove_roundtrip() {
        let cache = open_test_cache();
        assert!(load_accounts(&cache).unwrap().is_empty());

        add_account(&cache, test_account("a")).unwrap();
        add_account(
            &cache,
            Account {
                id: "b".into(),
                signature: Some("Best,\nAlex".into()),
                ..test_account("b")
            },
        )
        .unwrap();

        let listed = load_accounts(&cache).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "a"); // insertion order preserved
        assert_eq!(listed[1].signature.as_deref(), Some("Best,\nAlex"));

        // Duplicate id is rejected.
        assert!(add_account(&cache, test_account("a")).is_err());

        remove_account(&cache, "a").unwrap();
        let after_remove = load_accounts(&cache).unwrap();
        assert_eq!(after_remove.len(), 1);
        assert_eq!(after_remove[0].id, "b");

        // Removing an unknown id surfaces an error.
        assert!(remove_account(&cache, "missing").is_err());
    }

    #[test]
    fn update_replaces_fields() {
        let cache = open_test_cache();
        add_account(&cache, test_account("a")).unwrap();

        let renamed = Account {
            display_name: "Renamed".into(),
            email: "new@example.com".into(),
            signature: Some("sig".into()),
            ..test_account("a")
        };
        update_account(&cache, renamed).unwrap();

        let listed = load_accounts(&cache).unwrap();
        assert_eq!(listed[0].display_name, "Renamed");
        assert_eq!(listed[0].email, "new@example.com");
        assert_eq!(listed[0].signature.as_deref(), Some("sig"));

        // Updating an unknown id surfaces an error.
        assert!(update_account(&cache, test_account("missing")).is_err());
    }
}

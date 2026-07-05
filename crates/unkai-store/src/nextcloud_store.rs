//! Persistent Nextcloud-connection storage backed by the SQLCipher cache.
//!
//! Mirrors `account_store.rs`: connection metadata lives in the
//! encrypted cache so server URLs and usernames are no longer
//! readable from a plaintext file alongside the database (#155).
//! The app password itself stays in the OS keychain under
//! service `unkai-mail-nextcloud`.
//!
//! Connections remain a separate top-level record from mail
//! accounts because one Nextcloud often backs several mail
//! identities — see the doc comment on `NextcloudAccount`.

use rusqlite::params;
use tracing::{debug, info};
use unkai_core::UnkaiError;
use unkai_core::models::{DavSourceKind, NextcloudAccount, NextcloudCapabilities};

use crate::Cache;

/// Map the stored `kind` column to the enum.  Unknown values fall
/// back to `Nextcloud` — same posture as `#[serde(default)]` on the
/// struct: an old binary reading a newer DB shouldn't hard-fail.
fn kind_from_column(raw: &str) -> DavSourceKind {
    match raw {
        "dav" => DavSourceKind::Dav,
        "local" => DavSourceKind::Local,
        _ => DavSourceKind::Nextcloud,
    }
}

fn kind_to_column(kind: DavSourceKind) -> &'static str {
    match kind {
        DavSourceKind::Nextcloud => "nextcloud",
        DavSourceKind::Dav => "dav",
        DavSourceKind::Local => "local",
    }
}

/// Load all saved groupware connections (Nextcloud, generic DAV,
/// local — see `DavSourceKind`).  Empty list on first run.
pub fn load_accounts(cache: &Cache) -> Result<Vec<NextcloudAccount>, UnkaiError> {
    let conn = cache
        .conn()
        .map_err(|e| UnkaiError::Storage(format!("nextcloud load: {e}")))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, server_url, username, display_name, capabilities_json,
                    trusted_certs_json, kind, carddav_home, caldav_home
             FROM nextcloud_accounts
             ORDER BY rowid",
        )
        .map_err(|e| UnkaiError::Storage(format!("prepare nextcloud_accounts read: {e}")))?;
    let rows = stmt
        .query_map([], |r| {
            let caps_json: Option<String> = r.get(4)?;
            let capabilities = caps_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<NextcloudCapabilities>(s).ok());
            let trusted_certs_json: String = r.get(5)?;
            let trusted_certs = serde_json::from_str(&trusted_certs_json).unwrap_or_default();
            let kind_raw: String = r.get(6)?;
            Ok(NextcloudAccount {
                id: r.get(0)?,
                server_url: r.get(1)?,
                username: r.get(2)?,
                display_name: r.get(3)?,
                capabilities,
                trusted_certs,
                kind: kind_from_column(&kind_raw),
                carddav_home: r.get(7)?,
                caldav_home: r.get(8)?,
            })
        })
        .map_err(|e| UnkaiError::Storage(format!("query nextcloud_accounts: {e}")))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| UnkaiError::Storage(format!("row nextcloud_accounts: {e}")))?);
    }
    debug!("Loaded {} Nextcloud connection(s)", out.len());
    Ok(out)
}

/// Add a new Nextcloud connection, or overwrite one with the same id.
///
/// Overwrite semantics are deliberate: re-running Login Flow v2 for the
/// same logical server/user produces a fresh app password; we want to
/// update in place rather than accumulating stale rows.
pub fn upsert_account(cache: &Cache, acct: NextcloudAccount) -> Result<(), UnkaiError> {
    let conn = cache
        .conn()
        .map_err(|e| UnkaiError::Storage(format!("nextcloud upsert: {e}")))?;
    let caps_json = match &acct.capabilities {
        Some(c) => Some(
            serde_json::to_string(c)
                .map_err(|e| UnkaiError::Storage(format!("serialise capabilities: {e}")))?,
        ),
        None => None,
    };
    let trusted_certs_json = serde_json::to_string(&acct.trusted_certs)
        .map_err(|e| UnkaiError::Storage(format!("serialise trusted_certs: {e}")))?;
    conn.execute(
        "INSERT INTO nextcloud_accounts
            (id, server_url, username, display_name, capabilities_json,
             trusted_certs_json, kind, carddav_home, caldav_home)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
            server_url         = excluded.server_url,
            username           = excluded.username,
            display_name       = excluded.display_name,
            capabilities_json  = excluded.capabilities_json,
            trusted_certs_json = excluded.trusted_certs_json,
            kind               = excluded.kind,
            carddav_home       = excluded.carddav_home,
            caldav_home        = excluded.caldav_home",
        params![
            acct.id,
            acct.server_url,
            acct.username,
            acct.display_name,
            caps_json,
            trusted_certs_json,
            kind_to_column(acct.kind),
            acct.carddav_home,
            acct.caldav_home,
        ],
    )
    .map_err(|e| UnkaiError::Storage(format!("upsert nextcloud_accounts: {e}")))?;
    info!(
        "Stored Nextcloud connection '{}' ({}@{})",
        acct.id, acct.username, acct.server_url
    );
    Ok(())
}

/// Remove a Nextcloud connection by id.
pub fn remove_account(cache: &Cache, id: &str) -> Result<(), UnkaiError> {
    let conn = cache
        .conn()
        .map_err(|e| UnkaiError::Storage(format!("nextcloud remove: {e}")))?;
    let removed = conn
        .execute("DELETE FROM nextcloud_accounts WHERE id = ?1", params![id])
        .map_err(|e| UnkaiError::Storage(format!("delete nextcloud_accounts: {e}")))?;
    if removed == 0 {
        return Err(UnkaiError::Storage(format!(
            "no Nextcloud connection with id '{id}'"
        )));
    }
    info!("Removed Nextcloud connection '{id}'");
    Ok(())
}

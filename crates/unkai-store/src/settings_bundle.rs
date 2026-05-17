//! Portable settings bundle (#168).
//!
//! Serialises every user-visible preference into a single
//! `settings.json` blob the user can save to disk or to a connected
//! Nextcloud and later restore from. The bundle deliberately
//! excludes anything that depends on the local machine's secrets:
//!
//! - mail / Nextcloud passwords (live in the OS keychain)
//! - the SQLCipher master key, FIDO PRF wraps, the keychain
//!   envelope itself
//! - the encrypted cache database
//!
//! After importing on a fresh install the user still has to enter
//! mail and Nextcloud credentials — but every preference, theme,
//! folder→emoji mapping, signature, locale, etc. is restored.
//!
//! The bundle is schema-versioned so future fields can be added
//! without breaking older bundles or older clients.  Reading code
//! treats unknown top-level keys as forward-compatible (`#[serde
//! (default)]` on the struct), and the explicit `version` integer
//! lets us hard-fail on a future incompatible migration.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use unkai_core::UnkaiError;
use unkai_core::models::{Account, AppSettings};

use crate::Cache;

/// Schema version of the bundle JSON.  Bumped on incompatible
/// changes; importers refuse anything they can't recognise.
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// One mail account stripped of the local-only surface (passwords
/// live in the keychain so they're never in the bundle).  Trusted
/// TLS certs *are* included — they're a deliberate user choice and
/// useful to restore on a new machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleAccount {
    /// The full `Account` record from the user's config.  We let
    /// serde reuse the existing struct so adding a new account
    /// field automatically rides along in the bundle without a
    /// separate touch here.
    #[serde(flatten)]
    pub account: Account,
}

/// Top-level settings bundle.  Schema-versioned so future
/// migrations can refuse incompatible payloads instead of
/// silently mis-parsing them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsBundle {
    /// Schema version.  Always written as
    /// `BUNDLE_SCHEMA_VERSION`; importers reject higher values.
    pub version: u32,
    /// When the bundle was assembled.  Useful for the UI ("backed
    /// up 2 hours ago") and for the future "is the NC copy newer
    /// than my local one" prompt.
    pub exported_at: DateTime<Utc>,
    /// App-wide preferences (`AppSettings`).
    pub app_settings: AppSettings,
    /// Mail accounts (without passwords).
    pub accounts: Vec<BundleAccount>,
    /// Frontend-managed prefs that live in `localStorage` rather
    /// than the Rust config files (FIDO encryption toggle,
    /// trusted-senders list, locale pin, …).  Opaque map: the
    /// frontend collects + applies its own keys, the backend
    /// just round-trips it.
    pub local_storage: HashMap<String, String>,
}

impl Default for SettingsBundle {
    fn default() -> Self {
        Self {
            version: BUNDLE_SCHEMA_VERSION,
            exported_at: Utc::now(),
            app_settings: AppSettings::default(),
            accounts: Vec::new(),
            local_storage: HashMap::new(),
        }
    }
}

/// Build a bundle from the live in-process state.  The frontend
/// passes its `local_storage` snapshot; everything else is read
/// from the on-disk app-settings file plus the (already unlocked)
/// cache so the bundle reflects what's actually persisted.
pub fn build_bundle(
    cache: &Cache,
    local_storage: HashMap<String, String>,
) -> Result<SettingsBundle, UnkaiError> {
    let app_settings = crate::app_settings::load_settings()?;
    let accounts = crate::account_store::load_accounts(cache)?
        .into_iter()
        .map(|account| BundleAccount { account })
        .collect();
    Ok(SettingsBundle {
        version: BUNDLE_SCHEMA_VERSION,
        exported_at: Utc::now(),
        app_settings,
        accounts,
        local_storage,
    })
}

/// Serialise a bundle to a pretty-printed JSON string suitable for
/// `settings.json` on disk or on a Nextcloud.
pub fn serialise(bundle: &SettingsBundle) -> Result<String, UnkaiError> {
    serde_json::to_string_pretty(bundle)
        .map_err(|e| UnkaiError::Storage(format!("serialise settings bundle: {e}")))
}

/// Parse a JSON string back into a bundle.  Refuses anything
/// whose `version` is higher than this build understands.
pub fn parse(json: &str) -> Result<SettingsBundle, UnkaiError> {
    let bundle: SettingsBundle = serde_json::from_str(json)
        .map_err(|e| UnkaiError::Storage(format!("parse settings bundle: {e}")))?;
    if bundle.version > BUNDLE_SCHEMA_VERSION {
        return Err(UnkaiError::Storage(format!(
            "settings bundle version {} is newer than this app supports (max {}). \
             Update Unkai Mail and try again.",
            bundle.version, BUNDLE_SCHEMA_VERSION
        )));
    }
    Ok(bundle)
}

/// Apply a bundle to the local install.  Replaces `app_settings`
/// outright, upserts each account by id (no destructive delete:
/// accounts the user added since the bundle was made survive),
/// and returns the `local_storage` map so the frontend can write
/// it back.  Passwords aren't carried in the bundle, so accounts
/// imported from a fresh machine will need re-authentication on
/// first connect — surfaced as the standard "rejected app
/// password" path.
pub fn apply(cache: &Cache, bundle: SettingsBundle) -> Result<HashMap<String, String>, UnkaiError> {
    info!(
        "Applying settings bundle exported at {} (v{}) — {} account(s)",
        bundle.exported_at,
        bundle.version,
        bundle.accounts.len()
    );

    crate::app_settings::save_settings(&bundle.app_settings)?;

    let existing_ids: std::collections::HashSet<String> =
        crate::account_store::load_accounts(cache)?
            .into_iter()
            .map(|a| a.id)
            .collect();

    for entry in bundle.accounts {
        let imported = entry.account;
        if existing_ids.contains(&imported.id) {
            // Update preserves the row id and (importantly) the
            // OS-keychain password keyed on `imported.id` — that
            // entry was set when the user first signed in on
            // *this* machine and we never touch it.
            crate::account_store::update_account(cache, imported)?;
        } else {
            crate::account_store::add_account(cache, imported)?;
        }
    }

    debug!("Settings bundle applied");
    Ok(bundle.local_storage)
}

/// Soft validation.  Rejects empties + obvious malformed JSON
/// before we touch state — used by `apply()` callers that want a
/// pre-flight check.  Never panics.
pub fn looks_like_bundle(json: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(v) => v.get("version").is_some() && v.get("app_settings").is_some(),
        Err(_) => {
            warn!("looks_like_bundle: payload is not valid JSON");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default_bundle() {
        let mut local = HashMap::new();
        local.insert("unkai.keyEncryption".into(), "1".into());
        let bundle = SettingsBundle {
            local_storage: local.clone(),
            ..Default::default()
        };
        let json = serialise(&bundle).expect("serialise");
        let parsed = parse(&json).expect("parse");
        assert_eq!(parsed.version, BUNDLE_SCHEMA_VERSION);
        assert_eq!(
            parsed
                .local_storage
                .get("unkai.keyEncryption")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn rejects_future_version() {
        let payload =
            r#"{ "version": 9999, "app_settings": {}, "accounts": [], "local_storage": {} }"#;
        let err = parse(payload).expect_err("should reject future version");
        assert!(err.to_string().contains("newer than this app supports"));
    }

    #[test]
    fn looks_like_bundle_accepts_minimal() {
        let payload =
            r#"{ "version": 1, "app_settings": {}, "accounts": [], "local_storage": {} }"#;
        assert!(looks_like_bundle(payload));
    }

    #[test]
    fn looks_like_bundle_rejects_garbage() {
        assert!(!looks_like_bundle("not json"));
        assert!(!looks_like_bundle(r#"{ "hello": "world" }"#));
    }
}

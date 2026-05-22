//! Persistent app-wide preferences backed by a JSON file.
//!
//! Unlike `account_store`, which holds a list, this holds a single
//! `AppSettings` object. The file lives next to `accounts.json` at
//! `<config-dir>/unkai-mail/app_settings.json`.
//!
//! The `#[serde(default)]` on `AppSettings` means missing fields fall
//! back to `Default::default()` — adding a new setting in a future
//! version doesn't invalidate anyone's saved file.

use std::path::PathBuf;

use tracing::{debug, info};
use unkai_core::UnkaiError;
use unkai_core::models::AppSettings;

fn settings_file_path() -> Result<PathBuf, UnkaiError> {
    let data_dir = dirs::config_dir()
        .ok_or_else(|| UnkaiError::Storage("cannot determine config directory".into()))?;
    Ok(data_dir.join("unkai-mail").join("app_settings.json"))
}

/// Load the saved preferences, or `AppSettings::default()` on first run.
///
/// A missing file is the normal first-launch case — we return defaults
/// without writing anything. Callers that want the file to exist after
/// first launch can call `save_settings(&load_settings()?)` themselves;
/// we don't write implicitly here so tests don't accidentally touch the
/// user's real config dir.
pub fn load_settings() -> Result<AppSettings, UnkaiError> {
    let path = settings_file_path()?;

    if !path.exists() {
        debug!(
            "No app_settings file found at {}, using defaults",
            path.display()
        );
        return Ok(AppSettings::default());
    }

    let data = std::fs::read_to_string(&path)
        .map_err(|e| UnkaiError::Storage(format!("failed to read app_settings: {e}")))?;

    let settings: AppSettings = serde_json::from_str(&data)
        .map_err(|e| UnkaiError::Storage(format!("failed to parse app_settings: {e}")))?;

    info!("Loaded app settings from {}", path.display());
    Ok(settings)
}

/// Write the current preferences to disk, creating the parent dir
/// if needed.
pub fn save_settings(settings: &AppSettings) -> Result<(), UnkaiError> {
    let path = settings_file_path()?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| UnkaiError::Storage(format!("failed to create config dir: {e}")))?;
    }

    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| UnkaiError::Storage(format!("failed to serialize app_settings: {e}")))?;

    std::fs::write(&path, json)
        .map_err(|e| UnkaiError::Storage(format!("failed to write app_settings: {e}")))?;

    debug!("Saved app settings to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_file_path_is_valid() {
        let path = settings_file_path().expect("should resolve path");
        assert!(
            path.ends_with("unkai-mail/app_settings.json")
                || path.ends_with("unkai-mail\\app_settings.json")
        );
    }

    #[test]
    fn default_settings_roundtrip() {
        let s = AppSettings::default();
        let json = serde_json::to_string(&s).expect("serialize");
        let parsed: AppSettings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.background_sync_interval_secs, 60);
        assert!(parsed.minimize_to_tray);
    }

    #[test]
    fn missing_fields_fall_back_to_default() {
        // A forward-compat check: an older settings file that
        // predates a new field should still parse.
        let partial = r#"{ "minimize_to_tray": false }"#;
        let parsed: AppSettings = serde_json::from_str(partial).expect("deserialize");
        assert!(!parsed.minimize_to_tray);
        // The unspecified fields come from Default.
        assert!(parsed.background_sync_enabled);
        assert_eq!(parsed.background_sync_interval_secs, 60);
    }
}

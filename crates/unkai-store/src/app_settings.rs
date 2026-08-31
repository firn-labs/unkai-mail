//! Persistent app-wide preferences backed by a JSON file.
//!
//! Unlike `account_store`, which holds a list, this holds a single
//! `AppSettings` object. The file lives inside the active profile's
//! directory (`ProfilePaths::app_settings`, #531) — which is why
//! both functions take the file path as an argument instead of
//! computing one: the loader is a pure path-in/data-out function,
//! trivially testable against a temp dir, and the *choice* of
//! profile stays with the caller.
//!
//! The `#[serde(default)]` on `AppSettings` means missing fields fall
//! back to `Default::default()` — adding a new setting in a future
//! version doesn't invalidate anyone's saved file.

use std::path::Path;

use tracing::{debug, info};
use unkai_core::UnkaiError;
use unkai_core::models::AppSettings;

/// Load the saved preferences, or `AppSettings::default()` on first run.
///
/// A missing file is the normal first-launch case — we return defaults
/// without writing anything. Callers that want the file to exist after
/// first launch can call `save_settings(path, &load_settings(path)?)`
/// themselves; we don't write implicitly here so tests don't
/// accidentally touch a real config dir.
pub fn load_settings(path: &Path) -> Result<AppSettings, UnkaiError> {
    if !path.exists() {
        debug!(
            "No app_settings file found at {}, using defaults",
            path.display()
        );
        return Ok(AppSettings::default());
    }

    let data = std::fs::read_to_string(path)
        .map_err(|e| UnkaiError::Storage(format!("failed to read app_settings: {e}")))?;

    let settings: AppSettings = serde_json::from_str(&data)
        .map_err(|e| UnkaiError::Storage(format!("failed to parse app_settings: {e}")))?;

    info!("Loaded app settings from {}", path.display());
    Ok(settings)
}

/// Write the current preferences to disk, creating the parent dir
/// if needed.
pub fn save_settings(path: &Path, settings: &AppSettings) -> Result<(), UnkaiError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| UnkaiError::Storage(format!("failed to create config dir: {e}")))?;
    }

    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| UnkaiError::Storage(format!("failed to serialize app_settings: {e}")))?;

    std::fs::write(path, json)
        .map_err(|e| UnkaiError::Storage(format!("failed to write app_settings: {e}")))?;

    debug!("Saved app settings to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        // Test-only scratch path; uuid-suffixed and never created,
        // the test only needs a path that does not exist.
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        let path = std::env::temp_dir()
            .join("unkai-app-settings-test")
            .join(format!("{}.json", uuid::Uuid::new_v4()));
        let settings = load_settings(&path).expect("load from missing file");
        assert_eq!(settings.background_sync_interval_secs, 60);
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

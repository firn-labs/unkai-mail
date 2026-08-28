//! Persistence for the "Save settings to Nextcloud" feature (#168).
//!
//! Tracks two pieces of state:
//!   - **Sync target** — the id of the connected Nextcloud account
//!     the user designated as the recovery destination, or `None`
//!     if the feature is off.  Only one NC at a time can be the
//!     target — the toggle is mutually exclusive across NC rows.
//!   - **Pending flag** — set to `true` whenever a settings change
//!     happened but the most recent push to NC failed (offline,
//!     401, server down, …).  Cleared when the next push succeeds.
//!     Persisted so a crash mid-write or a quit-before-reconnect
//!     still surfaces the retry on next launch.
//!
//! Both pieces live in one tiny JSON file alongside
//! `app_settings.json` in the active profile's directory
//! (`ProfilePaths::settings_sync`, #531).  We deliberately don't
//! put them inside `AppSettings` itself because the bundle ships
//! `AppSettings` — and the sync target is a *local* choice that
//! shouldn't ride along when the user restores their settings on a
//! new device.  Like `app_settings`, load/save take the file path
//! as an argument — pure functions, caller picks the profile.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::debug;
use unkai_core::UnkaiError;

/// Local-only sync state.  Not part of the portable settings
/// bundle — see the module docstring for why.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsSyncState {
    /// Connected NC account id chosen as the recovery destination,
    /// or `None` when the user has the feature turned off.
    pub target_nc_id: Option<String>,
    /// `true` when an earlier settings change couldn't be pushed
    /// and is waiting for the next reachable-server window.
    pub pending: bool,
}

/// Load the saved sync state, or `Default::default()` if the file
/// doesn't exist yet.  Missing file is the normal first-run path.
pub fn load_state(path: &Path) -> Result<SettingsSyncState, UnkaiError> {
    if !path.exists() {
        return Ok(SettingsSyncState::default());
    }
    let data = std::fs::read_to_string(path)
        .map_err(|e| UnkaiError::Storage(format!("read settings_sync.json: {e}")))?;
    serde_json::from_str(&data)
        .map_err(|e| UnkaiError::Storage(format!("parse settings_sync.json: {e}")))
}

/// Persist the sync state to disk, creating the parent dir if
/// needed.  Called after every change to `target_nc_id` or
/// `pending`.
pub fn save_state(path: &Path, state: &SettingsSyncState) -> Result<(), UnkaiError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| UnkaiError::Storage(format!("create config dir: {e}")))?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| UnkaiError::Storage(format!("serialise settings_sync.json: {e}")))?;
    std::fs::write(path, json)
        .map_err(|e| UnkaiError::Storage(format!("write settings_sync.json: {e}")))?;
    debug!(
        "Saved settings_sync state: target={:?} pending={}",
        state.target_nc_id, state.pending
    );
    Ok(())
}

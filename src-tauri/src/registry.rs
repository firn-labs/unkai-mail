//! The multi-profile runtime (#533).
//!
//! One process hosts N independent profile contexts.  This module
//! owns the two maps that make that work:
//!
//!   * `contexts` — profile id → [`ProfileHandle`], the bundle of
//!     everything that exists once per profile (the `AppContext`,
//!     the MCP server, the settings-sync plumbing, the background
//!     task handles).
//!   * `windows` — window label → profile id.  Deliberately
//!     **mutable**: Tauri window labels are immutable, so chunk 4's
//!     switch-in-place works by remapping a label to a different
//!     profile here, never by relabeling the window.
//!
//! Every `#[tauri::command]` shim resolves its profile through
//! [`profile_ctx`] — one helper, no inline map lookups — and the
//! window-label→profile translation lives *only* in this crate;
//! `unkai-commands` never sees a window label (#476).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use tauri::State;
use unkai_commands::state::{AppContext, SettingsSyncNotify, SharedLocalStorage};
use unkai_core::UnkaiError;
use unkai_mcp::McpServer;
use unkai_store::ProfilePaths;

/// Everything that exists once per hosted profile.
///
/// Wraps the profile's [`AppContext`] (cache, settings, reminders,
/// notifier, profile identity) rather than competing with it — the
/// extra members here are the ones only the desktop shell knows
/// about: the MCP server lifecycle, the settings-sync worker's
/// mailbox, and the spawned background loops.
pub struct ProfileHandle {
    pub ctx: AppContext,
    /// This profile's MCP server (#438/#533).  One instance per
    /// profile so each serves only its own cache and settings.
    pub mcp: McpServer,
    /// The frontend's latest `localStorage` snapshot for this
    /// profile's windows (#168).
    pub local_storage: SharedLocalStorage,
    /// Wake channel for this profile's settings-sync worker.
    pub sync_notify: SettingsSyncNotify,
    /// Join handles for this profile's background loops.  Chunk 4
    /// aborts these when the last window of a profile closes; until
    /// then they live as long as the process.
    pub tasks: Mutex<Vec<tauri::async_runtime::JoinHandle<()>>>,
}

/// Managed state mapping windows to profiles and profiles to their
/// runtime contexts.  See the module docs for the two maps' roles.
pub struct ProfileRegistry {
    paths: ProfilePaths,
    /// The profile a plain launch opened (#531's startup
    /// resolution).  Doubles as the fallback for window labels
    /// that were never registered — standalone popouts created
    /// straight from the frontend land here until chunk 4 threads
    /// their parent's profile through the popout helpers.
    startup_profile: String,
    contexts: RwLock<HashMap<String, Arc<ProfileHandle>>>,
    windows: RwLock<HashMap<String, String>>,
    /// Last known unread total per profile, so the single tray
    /// icon can badge the aggregate across all profiles while each
    /// profile's windows still see only their own count.
    unread: RwLock<HashMap<String, u32>>,
}

impl ProfileRegistry {
    pub fn new(paths: ProfilePaths, startup_profile: String) -> Self {
        Self {
            paths,
            startup_profile,
            contexts: RwLock::new(HashMap::new()),
            windows: RwLock::new(HashMap::new()),
            unread: RwLock::new(HashMap::new()),
        }
    }

    pub fn paths(&self) -> &ProfilePaths {
        &self.paths
    }

    pub fn startup_profile_id(&self) -> &str {
        &self.startup_profile
    }

    /// Register (or replace) a profile's runtime context.
    pub fn insert_profile(&self, profile_id: &str, handle: Arc<ProfileHandle>) {
        self.contexts
            .write()
            .expect("profile contexts lock poisoned")
            .insert(profile_id.to_string(), handle);
    }

    /// Point a window label at a profile.  Called when a window is
    /// created — and, come chunk 4, when a switch-in-place remaps
    /// an existing window to another profile.
    pub fn map_window(&self, label: &str, profile_id: &str) {
        self.windows
            .write()
            .expect("profile windows lock poisoned")
            .insert(label.to_string(), profile_id.to_string());
    }

    /// Forget a window label (window destroyed).
    pub fn unmap_window(&self, label: &str) {
        self.windows
            .write()
            .expect("profile windows lock poisoned")
            .remove(label);
    }

    /// The profile a window label belongs to.  Unknown labels fall
    /// back to the startup profile so a popout created before its
    /// mapping is registered keeps working mid-series (chunk 4
    /// registers popouts at creation).
    pub fn profile_for_label(&self, label: &str) -> String {
        self.windows
            .read()
            .expect("profile windows lock poisoned")
            .get(label)
            .cloned()
            .unwrap_or_else(|| self.startup_profile.clone())
    }

    /// Resolve a window label to its profile's runtime context.
    pub fn handle_for_label(&self, label: &str) -> Result<Arc<ProfileHandle>, UnkaiError> {
        let profile_id = self.profile_for_label(label);
        self.contexts
            .read()
            .expect("profile contexts lock poisoned")
            .get(&profile_id)
            .cloned()
            .ok_or_else(|| {
                UnkaiError::Storage(format!("profile '{profile_id}' has no open context"))
            })
    }

    /// Resolve a profile id directly (non-window callers: tray,
    /// URI-scheme protocols resolving by label go through
    /// [`Self::handle_for_label`] instead).
    pub fn handle_for_profile(&self, profile_id: &str) -> Option<Arc<ProfileHandle>> {
        self.contexts
            .read()
            .expect("profile contexts lock poisoned")
            .get(profile_id)
            .cloned()
    }

    /// Every open profile context.  Used by whole-process fan-outs:
    /// the tray's "Check Mail Now", the aggregate unread badge.
    pub fn handles(&self) -> Vec<Arc<ProfileHandle>> {
        self.contexts
            .read()
            .expect("profile contexts lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// All window labels currently mapped to `profile_id`.
    pub fn labels_for_profile(&self, profile_id: &str) -> Vec<String> {
        self.windows
            .read()
            .expect("profile windows lock poisoned")
            .iter()
            .filter(|(_, pid)| pid.as_str() == profile_id)
            .map(|(label, _)| label.clone())
            .collect()
    }

    /// Record one profile's unread total and return the new
    /// aggregate across all profiles — what the single tray icon
    /// badges (planning decision on #530: one tray, combined
    /// count).
    pub fn record_unread(&self, profile_id: &str, total: u32) -> u32 {
        let mut unread = self.unread.write().expect("unread totals lock poisoned");
        unread.insert(profile_id.to_string(), total);
        unread.values().sum()
    }

    /// The current aggregate unread total without recording
    /// anything (tray repaints that don't stem from a count
    /// change, e.g. a logo-style swap).
    pub fn unread_sum(&self) -> u32 {
        self.unread
            .read()
            .expect("unread totals lock poisoned")
            .values()
            .sum()
    }
}

/// THE resolution helper (#533): every `#[tauri::command]` shim
/// that touches profile state calls this — and only this — to turn
/// its window into the profile's runtime context.  No shim does
/// map lookups inline.
pub fn profile_ctx(
    window: &tauri::Window,
    reg: &State<'_, ProfileRegistry>,
) -> Result<Arc<ProfileHandle>, UnkaiError> {
    reg.handle_for_label(window.label())
}

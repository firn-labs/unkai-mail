//! Process-wide application state, free of any Tauri `State<T>`
//! wrapper (#476).
//!
//! The desktop shell hands each of these to `Builder::manage` so the
//! `#[tauri::command]` shims can extract them; nothing here knows that.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::sync::RwLock;
use unkai_core::UnkaiError;
use unkai_core::models::AppSettings;
use unkai_store::Cache;

use crate::notify::UiNotifier;

/// Everything a long-running background task needs.
///
/// Commands mostly take `&Cache` directly — threading a whole context
/// through 156 cache-only functions would be noise.  The loops and the
/// handful of commands that both read state *and* talk back to the UI
/// take this instead.
#[derive(Clone)]
pub struct AppContext {
    pub cache: Cache,
    pub settings: SharedSettings,
    /// Fire-once bookkeeping for due event reminders.
    pub reminders: Arc<EventReminderState>,
    pub ui: Arc<dyn UiNotifier>,
}

/// Shared, mutable app preferences. Held as Tauri managed state so the
/// background loop can snapshot under a read lock on every tick while
/// `update_app_settings` swaps in a new value under the write lock.
pub type SharedSettings = Arc<RwLock<AppSettings>>;

/// Process-wide handle to the encrypted cache.  Populated once in
/// `main()` after `Cache::open_for_profile`, so non-IPC helpers can
/// reach the pool without every call site having to extract
/// `&Cache` and thread `&Cache` through itself.
pub static GLOBAL_CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();

pub fn global_cache() -> Result<&'static Cache, UnkaiError> {
    GLOBAL_CACHE
        .get()
        .ok_or_else(|| UnkaiError::Storage("cache not initialised yet".into()))
}

/// The profile this process is running as (#531).
///
/// Chunk 1 keeps the app single-profile, so "which profile does
/// this command act on" has exactly one answer for the whole
/// process — captured here once in `main()` right after the
/// registry resolves.  This is a deliberate sibling of
/// [`GLOBAL_CACHE`]: chunk 2 (#533) replaces both with the
/// `ProfileRegistry`'s per-window routing, so nothing new should
/// grow roots into this global beyond what per-window routing can
/// later serve.
pub struct ActiveProfile {
    pub id: String,
    pub paths: unkai_store::ProfilePaths,
}

impl ActiveProfile {
    /// This profile's `app_settings.json`.
    pub fn app_settings_file(&self) -> std::path::PathBuf {
        self.paths.app_settings(&self.id)
    }

    /// This profile's `settings_sync.json`.
    pub fn settings_sync_file(&self) -> std::path::PathBuf {
        self.paths.settings_sync(&self.id)
    }

    /// This profile's user-imported themes directory.
    pub fn themes_dir(&self) -> std::path::PathBuf {
        self.paths.themes_dir(&self.id)
    }
}

pub static ACTIVE_PROFILE: std::sync::OnceLock<ActiveProfile> = std::sync::OnceLock::new();

pub fn active_profile() -> Result<&'static ActiveProfile, UnkaiError> {
    ACTIVE_PROFILE
        .get()
        .ok_or_else(|| UnkaiError::Storage("active profile not initialised yet".into()))
}

/// In-memory state for the event-reminder pipeline.
///
/// `fired`: set of `(uid, minutes_before)` pairs we've already
///   pushed a notification for.  Pruned on each scan to drop
///   entries whose event has already started (the reminder is
///   moot once the event is in progress).
/// `dismissed`: UIDs the user explicitly silenced for the rest
///   of the meeting cycle (e.g. after clicking through to join
///   the room — surfaced via the `dismiss_event_reminder` IPC).
/// `snoozes`: UID → "fire again at this time" map populated by
///   the `snooze_event_reminder` IPC when the user picks one of
///   the snooze options on the popup window (#203 follow-up).
///   While a snooze is pending the scanner skips the event's
///   normal VALARM-driven reminders entirely; once `now`
///   crosses the snooze time the scanner fires a synthetic
///   reminder and removes the entry.
#[derive(Default)]
pub struct EventReminderState {
    pub fired: Mutex<HashSet<(String, i32)>>,
    pub dismissed: Mutex<HashSet<String>>,
    pub snoozes: Mutex<std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>>,
}

// ── App-settings commands ──────────────────────────────────────

/// Shared cache for the user's installed font families (#142).
/// Populated once at app startup on a blocking thread so the
/// compose toolbar's font picker reads instantly — re-running
/// font-kit's catalogue walk per dropdown open was visibly
/// laggy on machines with hundreds of fonts.
pub type SystemFontsCache = Arc<RwLock<Vec<String>>>;

/// Latest `localStorage` snapshot the frontend has shared with
/// us.  The auto-sync worker reads from here so it can assemble
/// a complete bundle without an additional IPC round-trip.
pub type SharedLocalStorage = Arc<RwLock<std::collections::HashMap<String, String>>>;

/// Notify channel used to wake the auto-sync worker.  Each
/// `notify_one()` call coalesces with any already-pending wakeup,
/// so a burst of settings changes still results in a single push
/// once the debounce window expires.
pub struct SettingsSyncNotify(pub Arc<tokio::sync::Notify>);

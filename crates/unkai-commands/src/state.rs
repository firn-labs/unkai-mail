//! Application state, free of any Tauri `State<T>` wrapper (#476).
//!
//! Since #533 the per-profile pieces ([`AppContext`] and the types it
//! bundles) live inside the desktop shell's `ProfileRegistry`, one
//! set per profile, resolved per window — only genuinely
//! machine-global state (the system-font cache) is still handed to
//! `Builder::manage` directly.  Nothing here knows either way.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::sync::RwLock;
use unkai_core::models::AppSettings;
use unkai_store::Cache;

use crate::notify::UiNotifier;

/// Everything a long-running background task needs.
///
/// Commands mostly take `&Cache` directly — threading a whole context
/// through 156 cache-only functions would be noise.  The loops and the
/// handful of commands that both read state *and* talk back to the UI
/// take this instead.
///
/// One `AppContext` exists per **profile** (#533): the cache, the
/// settings, the reminder bookkeeping, and the notifier are all
/// scoped to `profile`, so two profiles hosted by the same process
/// never share any of it.
#[derive(Clone)]
pub struct AppContext {
    pub cache: Cache,
    pub settings: SharedSettings,
    /// Fire-once bookkeeping for due event reminders.
    pub reminders: Arc<EventReminderState>,
    pub ui: Arc<dyn UiNotifier>,
    /// Which profile this context belongs to (id + storage layout).
    pub profile: Arc<ProfileInfo>,
}

/// Shared, mutable app preferences. Held as Tauri managed state so the
/// background loop can snapshot under a read lock on every tick while
/// `update_app_settings` swaps in a new value under the write lock.
pub type SharedSettings = Arc<RwLock<AppSettings>>;

/// A profile's identity plus its storage layout (#531/#533): the
/// id that keys the keychain entries and the [`ProfilePaths`]
/// resolver for its on-disk files.  Carried inside [`AppContext`]
/// so every command and loop knows which profile it acts on
/// without reaching for process globals.
pub struct ProfileInfo {
    pub id: String,
    pub paths: unkai_store::ProfilePaths,
}

impl ProfileInfo {
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
#[derive(Clone)]
pub struct SettingsSyncNotify(pub Arc<tokio::sync::Notify>);

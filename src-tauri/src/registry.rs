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

use tauri::{AppHandle, State};
use unkai_commands::background::{
    background_sync_loop, message_reminder_loop, prerender_inboxes_on_launch, settings_sync_worker,
    urlhaus_refresh_worker,
};
use unkai_commands::state::{
    AppContext, EventReminderState, ProfileInfo, SettingsSyncNotify, SharedLocalStorage,
    SharedSettings,
};
use unkai_core::UnkaiError;
use unkai_mcp::McpServer;
use unkai_store::{Cache, ProfilePaths, account_store, app_settings, credentials, settings_sync};

use crate::notifier::TauriNotifier;

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
    /// Join handles for this profile's background loops.  Closing
    /// a profile's last window deliberately does NOT abort these —
    /// background sync keeps running so the aggregate tray badge
    /// stays truthful (#535; native toasts are frontend-raised and
    /// pause until a window reopens).  They are aborted only by
    /// [`shutdown_profile_context`] (ahead of a `delete_profile`)
    /// or implicitly at app exit.
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
    /// The most recently focused *primary* window (label, profile
    /// id).  Every "raise the app" surface that has no profile of
    /// its own — tray clicks, a second launch, mailto deep links —
    /// lands on this window (#535).
    focused: RwLock<Option<(String, String)>>,
}

impl ProfileRegistry {
    pub fn new(paths: ProfilePaths, startup_profile: String) -> Self {
        Self {
            paths,
            startup_profile,
            contexts: RwLock::new(HashMap::new()),
            windows: RwLock::new(HashMap::new()),
            unread: RwLock::new(HashMap::new()),
            focused: RwLock::new(None),
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

    /// Remove a profile's runtime context and its tray-badge
    /// contribution.  Returns the handle so the caller can shut it
    /// down properly ([`shutdown_profile_context`] is the only
    /// intended caller).  `None` if the profile had no open
    /// context.
    pub fn remove_profile(&self, profile_id: &str) -> Option<Arc<ProfileHandle>> {
        self.unread
            .write()
            .expect("unread totals lock poisoned")
            .remove(profile_id);
        self.contexts
            .write()
            .expect("profile contexts lock poisoned")
            .remove(profile_id)
    }

    /// The profile's runtime context, if one is open.  Keyed by
    /// profile id — window-label resolution goes through
    /// [`Self::handle_for_label`] instead.
    pub fn context_for(&self, profile_id: &str) -> Option<Arc<ProfileHandle>> {
        self.contexts
            .read()
            .expect("profile contexts lock poisoned")
            .get(profile_id)
            .cloned()
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

    /// Forget a window label (window destroyed).  Also drops the
    /// focus bookmark when it pointed at this window, so the
    /// primary-window fallback never resolves to a dead label.
    pub fn unmap_window(&self, label: &str) {
        self.windows
            .write()
            .expect("profile windows lock poisoned")
            .remove(label);
        let mut focused = self.focused.write().expect("focused window lock poisoned");
        if focused.as_ref().is_some_and(|(l, _)| l == label) {
            *focused = None;
        }
    }

    /// Bookmark a primary window as the most recently focused one.
    /// Returns `true` when the focused *profile* changed — the
    /// caller uses that to persist `last_used` bookkeeping without
    /// rewriting `profiles.json` on every intra-profile focus flip.
    pub fn note_focused(&self, label: &str, profile_id: &str) -> bool {
        let mut focused = self.focused.write().expect("focused window lock poisoned");
        let changed = focused.as_ref().is_none_or(|(_, pid)| pid != profile_id);
        *focused = Some((label.to_string(), profile_id.to_string()));
        changed
    }

    /// The label of the most recently focused primary window, if
    /// any is bookmarked.
    pub fn last_focused_label(&self) -> Option<String> {
        self.focused
            .read()
            .expect("focused window lock poisoned")
            .as_ref()
            .map(|(label, _)| label.clone())
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

    /// The ids of every profile that at least one live window —
    /// primary or popout — is currently mapped to.  This is the
    /// `delete_profile` refusal set since #535: a window-less
    /// profile's context can be shut down cleanly
    /// ([`shutdown_profile_context`]) so only actually-visible
    /// profiles are undeletable.
    pub fn profiles_with_windows(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .windows
            .read()
            .expect("profile windows lock poisoned")
            .values()
            .cloned()
            .collect();
        ids.sort();
        ids.dedup();
        ids
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

/// Tear a profile's runtime context down completely (#535): abort
/// its background loops (and wait for them to actually finish),
/// stop its MCP listener, and drop the handle so the SQLCipher
/// pool releases `cache.db` — on Windows the file stays locked
/// until the last connection closes, and `delete_profile`'s wipe
/// would fail outright against a live pool.
///
/// The caller is responsible for making sure no window is still
/// mapped to the profile; commands arriving for it afterwards fail
/// with "profile has no open context" until something rebuilds the
/// handle (opening the profile's window again does).
pub async fn shutdown_profile_context(reg: &ProfileRegistry, profile_id: &str) {
    let Some(handle) = reg.remove_profile(profile_id) else {
        return;
    };
    let tasks: Vec<tauri::async_runtime::JoinHandle<()>> = {
        let mut guard = handle.tasks.lock().expect("profile tasks lock poisoned");
        guard.drain(..).collect()
    };
    for task in tasks {
        task.abort();
        // Await so the aborted future is dropped (releasing its
        // clones of the cache pool) before we return.
        let _ = task.await;
    }
    handle.mcp.shutdown().await;
    tracing::info!("profile '{profile_id}' runtime context shut down");
}

/// Construct one profile's complete runtime context: open its
/// cache, run the per-profile boot repairs, load its settings and
/// MCP token, and spawn its background loops.  Everything a
/// profile needs to live inside the process — chunk 4 calls this
/// again for each additional profile the user opens; today
/// `.setup()` calls it once for the startup profile.
///
/// Does NOT insert into the registry or map any window — the
/// caller decides when the handle becomes routable.
pub fn build_profile_handle(
    app: &AppHandle,
    profile_id: &str,
    paths: &ProfilePaths,
) -> Result<Arc<ProfileHandle>, UnkaiError> {
    // Open the profile's encrypted cache.  For the startup profile
    // a failure here is fatal (bubbled to `.setup()`): without the
    // cache the write-through path is broken and the user would
    // silently lose offline capability.
    let cache = Cache::open_for_profile(&paths.cache_db(profile_id), profile_id)?;

    // Scrub orphan cache rows left behind by removed accounts.
    // `cache.wipe_account(...)` runs on account removal, but if it
    // ever missed (crash, disk error, older build) the unified
    // inbox would surface envelopes whose owning account no longer
    // exists.  Running the scrub on every profile open guarantees
    // the shell never paints an orphan past the first frame.
    match account_store::load_accounts(&cache) {
        Ok(accounts) => {
            let active_ids: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
            if let Err(e) = cache.prune_orphan_accounts(&active_ids) {
                tracing::warn!("startup orphan-account prune failed: {e}");
            }
        }
        Err(e) => {
            tracing::warn!("skipping startup orphan-account prune — load_accounts failed: {e}")
        }
    }

    // One-time backfill for `addresses_json`.  The column was
    // added via ALTER TABLE with default `'[]'`; CardDAV's
    // delta-sync only re-pulls contacts that have changed in NC
    // since the last sync token, so unchanged ones kept the empty
    // default forever.  Self-narrowing: a fixed row's SELECT
    // condition no longer matches on subsequent opens.
    match cache.backfill_addresses(|raw| {
        let p = unkai_carddav::parse_vcard(raw).ok()?;
        Some(
            p.addresses
                .into_iter()
                .map(|a| unkai_core::models::ContactAddress {
                    kind: a.kind,
                    street: a.street,
                    locality: a.locality,
                    region: a.region,
                    postal_code: a.postal_code,
                    country: a.country,
                })
                .collect(),
        )
    }) {
        Ok(0) => {}
        Ok(n) => tracing::info!("contact backfill: rewrote addresses_json on {n} rows"),
        Err(e) => tracing::warn!("contact backfill failed: {e}"),
    }

    // This profile's preferences.  A missing file is fine on first
    // run — `load_settings` returns defaults.
    let settings = app_settings::load_settings(&paths.app_settings(profile_id)).unwrap_or_default();
    let shared_settings: SharedSettings = Arc::new(tokio::sync::RwLock::new(settings));

    // This profile's MCP bearer token (#438/#533).  Best-effort: a
    // broken keychain shouldn't stop the profile from opening, it
    // just leaves the server answering 401 until the user
    // re-generates a token.
    let mcp_token = credentials::get_mcp_token(profile_id).unwrap_or_else(|e| {
        tracing::warn!("could not read MCP token from keychain: {e}");
        None
    });

    let ctx = AppContext {
        cache: cache.clone(),
        settings: shared_settings.clone(),
        reminders: Arc::new(EventReminderState::default()),
        ui: Arc::new(TauriNotifier::new(app.clone(), profile_id.to_string())),
        profile: Arc::new(ProfileInfo {
            id: profile_id.to_string(),
            paths: paths.clone(),
        }),
    };

    let mcp = McpServer::new(cache, shared_settings, mcp_token);
    let local_storage: SharedLocalStorage =
        Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let sync_notify = SettingsSyncNotify(Arc::new(tokio::sync::Notify::new()));

    // ── This profile's background loops ─────────────────────────
    //
    // One set per profile context (#533), handles kept so chunk 4
    // can shut a profile's loops down when its last window closes.
    // `tauri::async_runtime::spawn` uses Tauri's managed runtime,
    // which is guaranteed to exist regardless of how the app was
    // started.
    let mut tasks = Vec::new();

    let bg_ctx = ctx.clone();
    tasks.push(tauri::async_runtime::spawn(async move {
        background_sync_loop(bg_ctx).await;
    }));

    // Own fixed-cadence loop, deliberately NOT gated on the
    // background-sync setting: a reminder the user set must fire
    // on time even with mail polling turned off (#415).
    let reminder_ctx = ctx.clone();
    tasks.push(tauri::async_runtime::spawn(async move {
        message_reminder_loop(reminder_ctx).await;
    }));

    // Launch-time prerender (#178): warm the message cache for the
    // newest INBOX envelopes whose body we haven't fetched yet, so
    // the first mail click paints from cache instead of waiting on
    // an IMAP round-trip.
    let prerender_ctx = ctx.clone();
    tasks.push(tauri::async_runtime::spawn(async move {
        prerender_inboxes_on_launch(&prerender_ctx).await;
    }));

    // Settings auto-sync worker (#168): wakes on notify_settings_
    // changed pings, debounces, and pushes the bundle to the
    // profile's chosen NC backup target.
    let sync_ctx = ctx.clone();
    let sync_storage = local_storage.clone();
    let worker_notify = sync_notify.0.clone();
    tasks.push(tauri::async_runtime::spawn(async move {
        settings_sync_worker(sync_ctx, sync_storage, worker_notify).await;
    }));
    // Kick the worker once so a pending recovery push from a
    // previous session (quit-while-offline) retries as soon as the
    // profile is up.  The worker no-ops cleanly if there's nothing
    // to do.
    if settings_sync::load_state(&ctx.profile.settings_sync_file())
        .map(|s| s.pending && s.target_nc_id.is_some())
        .unwrap_or(false)
    {
        sync_notify.0.notify_one();
    }

    // URLhaus link-safety refresh worker (#165).  One per profile
    // means N profiles download the hourly abuse.ch snapshot N
    // times — accepted for now; the optional shared-cache chunk
    // (#532) is where that dedupes into one machine-level worker.
    let urlhaus_cache = ctx.cache.clone();
    let urlhaus_settings = ctx.settings.clone();
    tasks.push(tauri::async_runtime::spawn(async move {
        urlhaus_refresh_worker(urlhaus_cache, urlhaus_settings).await;
    }));

    // MCP server (#438): one reconcile at boot brings the listener
    // up if `mcp_enabled` was saved on; afterwards the settings
    // commands re-reconcile on every change, so no polling loop.
    let mcp_boot = mcp.clone();
    tauri::async_runtime::spawn(async move {
        mcp_boot.reconcile().await;
    });

    Ok(Arc::new(ProfileHandle {
        ctx,
        mcp,
        local_storage,
        sync_notify,
        tasks: Mutex::new(tasks),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use unkai_commands::notify::UiNotifier;
    use unkai_core::models::Folder;

    /// Trait stub — the registry test exercises routing, not
    /// notification plumbing.
    struct NullNotifier;
    impl UiNotifier for NullNotifier {
        fn new_mail(&self, _: &unkai_commands::notify::NewMailPayload) {}
        fn mail_flags_updated(&self, _: &unkai_commands::notify::MailFlagsUpdatedPayload) {}
        fn outbox_updated(&self, _: &unkai_commands::notify::OutboxUpdatedPayload) {}
        fn calendars_updated(&self, _: &unkai_commands::notify::CalendarsUpdatedPayload) {}
        fn event_reminder(&self, _: &unkai_commands::notify::EventReminderPayload) {}
        fn message_reminder(
            &self,
            _: &unkai_commands::notify::MessageReminderPayload,
        ) -> Result<(), unkai_core::UnkaiError> {
            Ok(())
        }
        fn unread_total_changed(&self, _: u32) {}
        fn unread_by_account_changed(&self, _: &HashMap<String, u32>) {}
        fn custom_themes_changed(&self) {}
        fn profiles_changed(&self) {}
        fn apply_logo_style(&self, _: &str) -> Result<(), unkai_core::UnkaiError> {
            Ok(())
        }
    }

    /// A minimal in-memory ProfileHandle — everything a command
    /// body reaches through the resolution helper, without a Tauri
    /// runtime or the OS keychain.
    fn test_handle(profile_id: &str) -> Arc<ProfileHandle> {
        let cache = Cache::open_in_memory().expect("in-memory cache");
        let settings: SharedSettings = Arc::new(tokio::sync::RwLock::new(Default::default()));
        let ctx = AppContext {
            cache: cache.clone(),
            settings: settings.clone(),
            reminders: Arc::new(EventReminderState::default()),
            ui: Arc::new(NullNotifier),
            profile: Arc::new(ProfileInfo {
                id: profile_id.to_string(),
                paths: ProfilePaths::at_root(PathBuf::from("/tmp/unkai-registry-test")),
            }),
        };
        Arc::new(ProfileHandle {
            ctx,
            mcp: McpServer::new(cache, settings, None),
            local_storage: Arc::new(tokio::sync::RwLock::new(Default::default())),
            sync_notify: SettingsSyncNotify(Arc::new(tokio::sync::Notify::new())),
            tasks: Mutex::new(Vec::new()),
        })
    }

    /// The chunk-2 definition of done: two profile contexts open
    /// side by side in one process, a command body runs against
    /// each via the resolution path, and neither sees the other's
    /// data.
    #[test]
    fn two_profiles_resolve_without_cross_talk() {
        let reg = ProfileRegistry::new(
            ProfilePaths::at_root(PathBuf::from("/tmp/unkai-registry-test")),
            "profile-a".into(),
        );
        reg.insert_profile("profile-a", test_handle("profile-a"));
        reg.insert_profile("profile-b", test_handle("profile-b"));
        reg.map_window("main", "profile-a");
        reg.map_window("profile-b-window", "profile-b");

        // Write a folder into profile A's cache only — through the
        // same resolution the command shims use.
        let a = reg.handle_for_label("main").expect("profile A resolves");
        a.ctx
            .cache
            .upsert_folders(
                "acct-1",
                &[Folder {
                    name: "INBOX".into(),
                    delimiter: Some("/".into()),
                    attributes: vec![],
                    unread_count: Some(3),
                }],
            )
            .expect("seed profile A");

        // The same command body against each window's context: A
        // sees its folder, B sees nothing.
        let a = reg.handle_for_label("main").unwrap();
        let folders_a = unkai_commands::mail::get_cached_folders("acct-1".into(), &a.ctx.cache)
            .expect("A's folders");
        assert_eq!(folders_a.len(), 1, "profile A must see its own folder");

        let b = reg
            .handle_for_label("profile-b-window")
            .expect("profile B resolves");
        let folders_b = unkai_commands::mail::get_cached_folders("acct-1".into(), &b.ctx.cache)
            .expect("B's folders");
        assert!(
            folders_b.is_empty(),
            "profile B must not see profile A's data"
        );

        // Unknown labels (a popout created before chunk 4 registers
        // them) fall back to the startup profile.
        let fallback = reg.handle_for_label("compose-123").unwrap();
        assert_eq!(fallback.ctx.profile.id, "profile-a");

        // The shared tray badges the aggregate; each profile's own
        // total stays separate.
        assert_eq!(reg.record_unread("profile-a", 3), 3);
        assert_eq!(reg.record_unread("profile-b", 2), 5);
        assert_eq!(reg.unread_sum(), 5);
    }
}

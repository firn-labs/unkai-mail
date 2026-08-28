//! Profile management: CRUD on the machine-global profile registry
//! plus the startup-mode setting (#534).
//!
//! Mirrors `ui/src/lib/api/profiles.ts`.
//!
//! Everything here operates on `profiles.json` and the profile
//! directories through [`ProfilePaths`] — the commands never touch a
//! *running* profile context.  The desktop shell resolves the caller's
//! window to its profile (for [`get_current_profile`]) and tells
//! [`delete_profile`] which profiles currently have an open runtime
//! context; policy for both lives here so it is testable without a
//! Tauri runtime.
//!
//! Every mutation ends with [`UiNotifier::profiles_changed`] so every
//! window's profile list repaints — the registry is machine-global,
//! and with chunk 4 (#535) windows of *other* profiles show it too.

use tracing::warn;
use unkai_core::UnkaiError;
use unkai_store::profiles::{ProfileIcon, ProfileMeta, ProfilesFile, StartupMode};
use unkai_store::{
    Cache, ProfilePaths, account_store, cache, credentials, nextcloud_store, profiles,
};

use crate::notify::UiNotifier;
use crate::state::ProfileInfo;

/// Return every profile in the registry, in registry (creation)
/// order.  The frontend's `profileStore` keeps this as its single
/// source of truth for the list.
pub fn list_profiles(paths: &ProfilePaths) -> Result<Vec<ProfileMeta>, UnkaiError> {
    Ok(load_registry(paths)?.profiles)
}

/// The id of the profile the calling window belongs to.  The shim
/// resolves the window through the registry and hands the context's
/// [`ProfileInfo`] here — the body exists so the command keeps the
/// 1:1 module mirror with `api/profiles.ts`.
pub fn get_current_profile(profile: &ProfileInfo) -> String {
    profile.id.clone()
}

/// Create a new profile: registry entry first (crash-safe, same
/// ordering rationale as `profiles::ensure_registry` — the persisted
/// id is what makes a retry target the same directory), then the
/// profile's directory and a fresh SQLCipher-keyed empty cache.
///
/// The new profile starts with zero accounts; opening it for the
/// first time (chunk 4, #535) lands in the account-setup flow.
pub fn create_profile(
    ui: &dyn UiNotifier,
    name: String,
    icon: ProfileIcon,
    paths: &ProfilePaths,
) -> Result<ProfileMeta, UnkaiError> {
    let name = validated_name(&name)?;
    validated_icon(&icon)?;

    let meta = ProfileMeta::new(name, icon);
    let entry = meta.clone();
    // All registry read-modify-writes are serialised (#535) — a
    // concurrent focus-driven `last_used` write must not clobber
    // the freshly created row.
    profiles::update_registry(paths, move |registry| {
        registry.profiles.push(entry);
        Ok(())
    })?;

    // Mint the profile's master key and create its directory +
    // empty cache.db eagerly.  If this fails the profile stays
    // listed and the open simply retries on first use — strictly
    // better than the reverse order, where a registry-save failure
    // would strand a keyed cache nobody can reach.
    let cache = Cache::open_for_profile(&paths.cache_db(&meta.id), &meta.id)?;
    drop(cache);

    ui.profiles_changed();
    Ok(meta)
}

/// Rename and/or re-icon a profile.  `None` fields stay untouched.
pub fn update_profile(
    ui: &dyn UiNotifier,
    id: String,
    name: Option<String>,
    icon: Option<ProfileIcon>,
    paths: &ProfilePaths,
) -> Result<ProfileMeta, UnkaiError> {
    let name = name.map(|n| validated_name(&n)).transpose()?;
    if let Some(icon) = &icon {
        validated_icon(icon)?;
    }

    let updated = profiles::update_registry(paths, move |registry| {
        let profile = registry
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| UnkaiError::Other(format!("no profile with id '{id}'")))?;
        if let Some(name) = name {
            profile.name = name;
        }
        if let Some(icon) = icon {
            profile.icon = icon;
        }
        Ok(profile.clone())
    })?;

    ui.profiles_changed();
    Ok(updated)
}

/// Pre-flight the deletion policy without deleting anything
/// (#535).  The desktop shell calls this before shutting down a
/// window-less profile's runtime context, so a refusal (last
/// profile, still visible in a window, …) never costs a context
/// teardown-and-rebuild.
pub fn ensure_deletable(
    id: &str,
    active_profile_id: &str,
    open_profile_ids: &[String],
    paths: &ProfilePaths,
) -> Result<(), UnkaiError> {
    let registry = load_registry(paths)?;
    check_deletable(&registry, id, active_profile_id, open_profile_ids)
}

/// Delete a profile and securely destroy its local data.
///
/// `active_profile_id` is the calling window's profile;
/// `open_profile_ids` are the profiles some live window is mapped
/// to (#535 — a window-less profile's context is shut down by the
/// shell before this runs, so its cache files are unlocked for the
/// wipe below).  Both are refused, as is the last remaining
/// profile.
///
/// Destruction order follows the account-removal precedent: secrets
/// and files go first, the registry row last, so "this profile
/// exists" stays truthful right up until cleanup completed — a
/// failure mid-way leaves a retryable profile, never a ghost entry.
pub fn delete_profile(
    ui: &dyn UiNotifier,
    id: String,
    active_profile_id: &str,
    open_profile_ids: &[String],
    paths: &ProfilePaths,
) -> Result<(), UnkaiError> {
    let registry = load_registry(paths)?;
    check_deletable(&registry, &id, active_profile_id, open_profile_ids)?;

    // Enumerate the profile's accounts while its cache still
    // exists, and clear their keychain entries.  Best-effort: a
    // FIDO-locked profile (or a corrupt DB) can't be enumerated —
    // the wipe below still destroys the data, at the cost of
    // orphaned credential entries in the OS keychain.
    let db_path = paths.cache_db(&id);
    if db_path.exists() {
        match Cache::open_for_profile(&db_path, &id) {
            Ok(cache) => {
                delete_account_credentials(&cache, &id);
                // Close the pool before wiping — on Windows the
                // unlink fails outright while handles are open.
                drop(cache);
            }
            Err(e) => warn!(
                "could not open profile '{id}' to enumerate accounts — \
                 its per-account keychain entries are not cleaned up: {e}"
            ),
        }
        // Random-overwrite + unlink of cache.db and its WAL/SHM
        // sidecars, same pattern as the failed-unlock wipe policy.
        cache::wipe_cache_files(&db_path)?;
    }

    // Keys after the data: a deleted key with a live DB would be
    // unrecoverable, a wiped DB with a leftover key is only an
    // orphan entry (and these deleters are retried-on-next-call
    // tolerant anyway).
    if let Err(e) = cache::key::delete_master_key(&id) {
        warn!("could not delete master key for profile '{id}': {e}");
    }
    if let Err(e) = credentials::delete_mcp_token(&id) {
        warn!("could not delete MCP token for profile '{id}': {e}");
    }

    // The rest of the profile dir (app_settings.json, themes/)
    // carries no secrets — plain removal is enough.
    let dir = paths.profile_dir(&id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| {
            UnkaiError::Storage(format!("remove profile dir {}: {e}", dir.display()))
        })?;
    }

    // Re-load under the registry write lock (#535): the wipe above
    // took real time, and a concurrent `last_used` write must not
    // be clobbered by our pre-wipe snapshot.
    profiles::update_registry(paths, |registry| {
        remove_from_registry(registry, &id);
        Ok(())
    })?;

    ui.profiles_changed();
    Ok(())
}

/// Which profile(s) the app opens at launch.
pub fn get_startup_mode(paths: &ProfilePaths) -> Result<StartupMode, UnkaiError> {
    Ok(load_registry(paths)?.startup)
}

/// Persist a new startup mode.  A `Fixed` id must name an existing
/// profile — the resolution in `ProfilesFile::startup_profile` would
/// fall back gracefully anyway, but silently accepting a dangling id
/// would leave the settings UI showing a choice that never applies.
pub fn set_startup_mode(
    ui: &dyn UiNotifier,
    mode: StartupMode,
    paths: &ProfilePaths,
) -> Result<(), UnkaiError> {
    profiles::update_registry(paths, move |registry| {
        if let StartupMode::Fixed(id) = &mode
            && !registry.profiles.iter().any(|p| p.id == *id)
        {
            return Err(UnkaiError::Other(format!(
                "cannot fix startup on unknown profile '{id}'"
            )));
        }
        registry.startup = mode;
        Ok(())
    })?;

    ui.profiles_changed();
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────

fn load_registry(paths: &ProfilePaths) -> Result<ProfilesFile, UnkaiError> {
    profiles::load_profiles(&paths.profiles_json())
}

/// A usable profile name: non-empty after trimming, and stored
/// trimmed so the picker never renders invisible whitespace.
fn validated_name(name: &str) -> Result<String, UnkaiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(UnkaiError::Other("profile name must not be empty".into()));
    }
    Ok(trimmed.to_string())
}

/// Both icon kinds carry their value as a string — an empty one
/// would render an invisible picker bubble.
fn validated_icon(icon: &ProfileIcon) -> Result<(), UnkaiError> {
    let value = match icon {
        ProfileIcon::Emoji(v) | ProfileIcon::Named(v) => v,
    };
    if value.trim().is_empty() {
        return Err(UnkaiError::Other("profile icon must not be empty".into()));
    }
    Ok(())
}

/// The deletion policy, separated from the I/O so it is unit-
/// testable: the profile must exist, must not be the last one, and
/// must not be running (the caller's own profile, or any profile
/// with an open runtime context).
fn check_deletable(
    registry: &ProfilesFile,
    id: &str,
    active_profile_id: &str,
    open_profile_ids: &[String],
) -> Result<(), UnkaiError> {
    if !registry.profiles.iter().any(|p| p.id == id) {
        return Err(UnkaiError::Other(format!("no profile with id '{id}'")));
    }
    if registry.profiles.len() <= 1 {
        return Err(UnkaiError::Other(
            "the last remaining profile cannot be deleted".into(),
        ));
    }
    if id == active_profile_id {
        return Err(UnkaiError::Other(
            "this window is using the profile — switch profiles before deleting it".into(),
        ));
    }
    if open_profile_ids.iter().any(|open| open == id) {
        return Err(UnkaiError::Other(
            "the profile is open in another window — close it before deleting".into(),
        ));
    }
    Ok(())
}

/// Drop a profile's registry row and every reference to it: the
/// `last_used` order, and a `Fixed` startup pin (which falls back
/// to `LastUsed` rather than pointing at a ghost).
fn remove_from_registry(registry: &mut ProfilesFile, id: &str) {
    registry.profiles.retain(|p| p.id != id);
    registry.last_used.retain(|used| used != id);
    if matches!(&registry.startup, StartupMode::Fixed(fixed) if fixed == id) {
        registry.startup = StartupMode::LastUsed;
    }
}

/// Clear every keychain entry belonging to the profile's accounts:
/// IMAP password, OpenPGP key + passphrase, and S/MIME identity +
/// passphrase per mail account, plus the app password per Nextcloud
/// / DAV connection.  Best-effort throughout — each deleter is a
/// no-op on a missing entry, and a keychain hiccup on one account
/// must not stop the sweep (or the wipe that follows).
fn delete_account_credentials(cache: &Cache, profile_id: &str) {
    match account_store::load_accounts(cache) {
        Ok(accounts) => {
            for account in &accounts {
                let id = &account.id;
                for (what, result) in [
                    ("IMAP password", credentials::delete_imap_password(id)),
                    ("PGP private key", credentials::delete_pgp_private_key(id)),
                    ("PGP passphrase", credentials::delete_pgp_passphrase(id)),
                    (
                        "S/MIME identity",
                        credentials::delete_smime_private_cert(id),
                    ),
                    (
                        "S/MIME passphrase",
                        credentials::delete_smime_passphrase(id),
                    ),
                ] {
                    if let Err(e) = result {
                        warn!(
                            "profile '{profile_id}': failed to delete {what} for account '{id}': {e}"
                        );
                    }
                }
            }
        }
        Err(e) => warn!(
            "profile '{profile_id}': could not enumerate mail accounts for credential cleanup: {e}"
        ),
    }
    match nextcloud_store::load_accounts(cache) {
        Ok(connections) => {
            for nc in &connections {
                if let Err(e) = credentials::delete_nextcloud_password(&nc.id) {
                    warn!(
                        "profile '{profile_id}': failed to delete Nextcloud password for '{}': {e}",
                        nc.id
                    );
                }
            }
        }
        Err(e) => warn!(
            "profile '{profile_id}': could not enumerate Nextcloud connections for credential cleanup: {e}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// No-op notifier so the file-only commands can run in tests.
    struct NullNotifier;
    impl UiNotifier for NullNotifier {
        fn new_mail(&self, _: &crate::notify::NewMailPayload) {}
        fn mail_flags_updated(&self, _: &crate::notify::MailFlagsUpdatedPayload) {}
        fn outbox_updated(&self, _: &crate::notify::OutboxUpdatedPayload) {}
        fn calendars_updated(&self, _: &crate::notify::CalendarsUpdatedPayload) {}
        fn event_reminder(&self, _: &crate::notify::EventReminderPayload) {}
        fn message_reminder(
            &self,
            _: &crate::notify::MessageReminderPayload,
        ) -> Result<(), UnkaiError> {
            Ok(())
        }
        fn unread_total_changed(&self, _: u32) {}
        fn unread_by_account_changed(&self, _: &std::collections::HashMap<String, u32>) {}
        fn custom_themes_changed(&self) {}
        fn profiles_changed(&self) {}
        fn apply_logo_style(&self, _: &str) -> Result<(), UnkaiError> {
            Ok(())
        }
    }

    /// Unique temp dir removed on drop — same shape as the
    /// migration tests in `unkai_store::profiles`, and for the
    /// same reason (no tempdir crate in the workspace).
    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join("unkai-profiles-cmd-test")
                .join(uuid_ish());
            std::fs::create_dir_all(&dir).expect("create temp root");
            Self(dir)
        }
        fn paths(&self) -> ProfilePaths {
            ProfilePaths::at_root(self.0.clone())
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Unique-enough directory name without pulling `uuid` into
    /// this crate's dependency list for a test helper.
    fn uuid_ish() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        format!("{nanos}-{:p}", &nanos)
    }

    fn seeded_registry(paths: &ProfilePaths, names: &[&str]) -> ProfilesFile {
        let registry = ProfilesFile {
            profiles: names
                .iter()
                .map(|n| ProfileMeta::new((*n).into(), ProfileIcon::default()))
                .collect(),
            ..Default::default()
        };
        profiles::save_profiles(&paths.profiles_json(), &registry).expect("seed registry");
        registry
    }

    #[test]
    fn list_returns_registry_order() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();
        seeded_registry(&paths, &["Work", "Private"]);
        let listed = list_profiles(&paths).expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "Work");
        assert_eq!(listed[1].name, "Private");
    }

    #[test]
    fn update_renames_and_reicons_and_persists() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();
        let seeded = seeded_registry(&paths, &["Work"]);
        let id = seeded.profiles[0].id.clone();

        let updated = update_profile(
            &NullNotifier,
            id.clone(),
            Some("  Job  ".into()),
            Some(ProfileIcon::Emoji("🦊".into())),
            &paths,
        )
        .expect("update");
        assert_eq!(updated.name, "Job", "name is stored trimmed");
        assert_eq!(updated.icon, ProfileIcon::Emoji("🦊".into()));

        let reloaded = list_profiles(&paths).expect("reload");
        assert_eq!(reloaded[0].name, "Job");

        // None fields leave the stored values untouched.
        let untouched =
            update_profile(&NullNotifier, id, None, None, &paths).expect("no-op update");
        assert_eq!(untouched.name, "Job");
        assert_eq!(untouched.icon, ProfileIcon::Emoji("🦊".into()));
    }

    #[test]
    fn update_rejects_empty_names_and_unknown_ids() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();
        let seeded = seeded_registry(&paths, &["Work"]);
        let id = seeded.profiles[0].id.clone();

        update_profile(&NullNotifier, id.clone(), Some("   ".into()), None, &paths)
            .expect_err("blank name must be rejected");
        update_profile(
            &NullNotifier,
            id,
            None,
            Some(ProfileIcon::Named("".into())),
            &paths,
        )
        .expect_err("empty icon value must be rejected");
        update_profile(
            &NullNotifier,
            "ghost".into(),
            Some("X".into()),
            None,
            &paths,
        )
        .expect_err("unknown id must be rejected");
    }

    #[test]
    fn startup_mode_roundtrips_and_validates_fixed_ids() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();
        let seeded = seeded_registry(&paths, &["Work", "Private"]);
        let id = seeded.profiles[1].id.clone();

        assert_eq!(
            get_startup_mode(&paths).expect("default"),
            StartupMode::LastUsed
        );
        set_startup_mode(&NullNotifier, StartupMode::Fixed(id.clone()), &paths).expect("set fixed");
        assert_eq!(
            get_startup_mode(&paths).expect("get"),
            StartupMode::Fixed(id)
        );
        set_startup_mode(&NullNotifier, StartupMode::Fixed("ghost".into()), &paths)
            .expect_err("dangling fixed id must be rejected");
        set_startup_mode(&NullNotifier, StartupMode::All, &paths).expect("set all");
        assert_eq!(get_startup_mode(&paths).expect("get"), StartupMode::All);
    }

    #[test]
    fn deletion_policy_refuses_last_active_and_open_profiles() {
        let mut registry = ProfilesFile {
            profiles: vec![
                ProfileMeta::new("Work".into(), ProfileIcon::default()),
                ProfileMeta::new("Private".into(), ProfileIcon::default()),
            ],
            ..Default::default()
        };
        let work = registry.profiles[0].id.clone();
        let private = registry.profiles[1].id.clone();

        check_deletable(&registry, "ghost", &work, &[work.clone()])
            .expect_err("unknown id refused");
        check_deletable(&registry, &work, &work, &[work.clone()])
            .expect_err("the window's own profile is refused");
        check_deletable(&registry, &private, &work, &[work.clone(), private.clone()])
            .expect_err("a profile with an open context is refused");
        check_deletable(&registry, &private, &work, &[work.clone()])
            .expect("a closed, non-active, non-last profile is deletable");

        registry.profiles.remove(1);
        check_deletable(&registry, &work, "other", &[]).expect_err("the last profile is refused");
    }

    #[test]
    fn registry_removal_prunes_every_reference() {
        let mut registry = ProfilesFile {
            profiles: vec![
                ProfileMeta::new("Work".into(), ProfileIcon::default()),
                ProfileMeta::new("Private".into(), ProfileIcon::default()),
            ],
            ..Default::default()
        };
        let work = registry.profiles[0].id.clone();
        let private = registry.profiles[1].id.clone();
        registry.last_used = vec![private.clone(), work.clone()];
        registry.startup = StartupMode::Fixed(private.clone());

        remove_from_registry(&mut registry, &private);
        assert_eq!(registry.profiles.len(), 1);
        assert_eq!(registry.profiles[0].id, work);
        assert_eq!(registry.last_used, vec![work]);
        assert_eq!(
            registry.startup,
            StartupMode::LastUsed,
            "a Fixed pin on the deleted profile falls back instead of dangling"
        );
    }
}

//! Profile registry + one-time legacy-layout migration (#531).
//!
//! Profiles are a **filesystem dimension, not a schema dimension**:
//! each profile owns a full `cache.db` (existing schema, its own
//! SQLCipher master key) plus its settings files under
//! `profiles/<id>/` — see [`crate::paths`] for the layout.  The
//! registry file `profiles.json` is the only machine-global piece:
//! it lists the profiles, remembers usage order, and records how
//! startup should pick a profile.
//!
//! # Migration
//!
//! Installs that predate profiles keep everything flat under the
//! config root.  [`ensure_registry`] converges any such install to
//! the profile layout, and is deliberately built out of steps that
//! are each independently idempotent so a crash at *any* point
//! resumes cleanly on the next boot:
//!
//! 1. `profiles.json` is written **first** (minting the default
//!    profile's UUID).  The persisted id is what makes a re-run
//!    after a crash target the *same* profile directory instead of
//!    minting a fresh UUID and stranding half-moved files.
//! 2. Legacy files are moved with `fs::rename` (atomic on the same
//!    volume; the profile dir is inside the config root, so it
//!    always is).  Each move is "skip if the source is gone" —
//!    re-runs pick up whatever a crash left behind, including
//!    SQLite `-wal` / `-shm` sidecars.
//! 3. Stored absolute theme paths inside the moved
//!    `app_settings.json` are rewritten to the new themes dir.
//! 4. The keychain master key moves from the singleton `master-key`
//!    account to `master-key:<id>` — write-new, verify, delete-old,
//!    never delete-first (see [`crate::cache::key`]).
//!
//! Everything runs strictly **before** the DB is opened: SQLCipher
//! files must never be renamed while a pool holds them open.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use unkai_core::UnkaiError;

use crate::cache::key;
use crate::paths::ProfilePaths;

/// Schema version of `profiles.json`.  Bumped on incompatible
/// changes so a future build can migrate (or refuse) explicitly.
pub const PROFILES_SCHEMA_VERSION: u32 = 1;

/// A profile's picker icon: either a user-chosen emoji or the name
/// of one of the frontend's predefined icons (an `IconName` from
/// `ui/src/lib/Icon.svelte`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ProfileIcon {
    Emoji(String),
    Named(String),
}

impl Default for ProfileIcon {
    fn default() -> Self {
        // "contacts" is a registered IconName today; the management
        // UI (chunk 3, #534) lets the user change it.
        ProfileIcon::Named("contacts".into())
    }
}

/// One profile's registry entry.  Everything else about a profile
/// lives inside its own directory — this is only what the picker
/// and the window chrome need before the profile's DB is open.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileMeta {
    /// UUID string; doubles as the directory name under
    /// `profiles/` and the keychain suffix `master-key:<id>`.
    pub id: String,
    pub name: String,
    pub icon: ProfileIcon,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
}

impl Default for ProfileMeta {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            icon: ProfileIcon::default(),
            created_at: Utc::now(),
            last_used_at: Utc::now(),
        }
    }
}

impl ProfileMeta {
    /// Mint the profile every install starts with — both fresh
    /// installs and migrated legacy ones.
    pub fn new_default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Default".into(),
            ..Default::default()
        }
    }
}

/// Which profile(s) the app opens at launch.  Chunk 1 only ever
/// resolves this to the single existing profile; the startup-mode
/// picker UI lands with the window work (chunk 4, #535).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", content = "id", rename_all = "snake_case")]
pub enum StartupMode {
    /// Always open this one profile.
    Fixed(String),
    /// Open whatever was focused most recently.
    #[default]
    LastUsed,
    /// Open a window for every profile.
    All,
}

/// The `profiles.json` registry.  `#[serde(default)]` keeps old
/// files parseable when fields are added, same resilience story as
/// `app_settings.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilesFile {
    pub version: u32,
    pub profiles: Vec<ProfileMeta>,
    pub startup: StartupMode,
    /// Profile ids, most recently used first.  Drives `LastUsed`
    /// startup and (later) the profile switcher's ordering.
    pub last_used: Vec<String>,
}

impl Default for ProfilesFile {
    fn default() -> Self {
        Self {
            version: PROFILES_SCHEMA_VERSION,
            profiles: Vec::new(),
            startup: StartupMode::default(),
            last_used: Vec::new(),
        }
    }
}

impl ProfilesFile {
    /// Resolve which profile a plain launch should open.  `All`
    /// mode still needs a *primary* window (tray clicks, file
    /// handlers) — that's the most recently used one, same as
    /// `LastUsed`.  Falls back to the first profile whenever the
    /// referenced id no longer exists.  `None` only for an empty
    /// registry, which [`ensure_registry`] never produces.
    pub fn startup_profile(&self) -> Option<&ProfileMeta> {
        let by_id = |id: &str| self.profiles.iter().find(|p| p.id == id);
        match &self.startup {
            StartupMode::Fixed(id) => by_id(id).or_else(|| self.profiles.first()),
            StartupMode::LastUsed | StartupMode::All => self
                .last_used
                .iter()
                .find_map(|id| by_id(id))
                .or_else(|| self.profiles.first()),
        }
    }
}

/// Load the registry from `path`.
pub fn load_profiles(path: &Path) -> Result<ProfilesFile, UnkaiError> {
    let data = std::fs::read_to_string(path)
        .map_err(|e| UnkaiError::Storage(format!("read profiles.json: {e}")))?;
    serde_json::from_str(&data)
        .map_err(|e| UnkaiError::Storage(format!("parse profiles.json: {e}")))
}

/// Persist the registry to `path`, creating the parent dir if
/// needed.
pub fn save_profiles(path: &Path, file: &ProfilesFile) -> Result<(), UnkaiError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| UnkaiError::Storage(format!("create config dir: {e}")))?;
    }
    let json = serde_json::to_string_pretty(file)
        .map_err(|e| UnkaiError::Storage(format!("serialise profiles.json: {e}")))?;
    std::fs::write(path, json).map_err(|e| UnkaiError::Storage(format!("write profiles.json: {e}")))
}

/// Load-or-create the registry and converge the install to the
/// profile layout.  Runs on every boot, strictly before any
/// profile's DB is opened; both an already-migrated install and a
/// fresh one no-op past the migration steps.
pub fn ensure_registry(paths: &ProfilePaths) -> Result<ProfilesFile, UnkaiError> {
    let registry = ensure_registry_files(paths)?;
    // Keychain last, and fatally: if the master key can't reach its
    // per-profile entry, opening the migrated DB would mint a fresh
    // key, fail to decrypt, and trip the wipe-and-recreate path —
    // refusing to boot is strictly safer.
    key::migrate_legacy_master_key(&registry.profiles[0].id)?;
    Ok(registry)
}

/// The filesystem half of [`ensure_registry`]: registry creation
/// plus the legacy file moves, with no keychain access.  Split out
/// so the migration unit tests below can run against a temp dir
/// without ever touching the developer's real OS keychain.
fn ensure_registry_files(paths: &ProfilePaths) -> Result<ProfilesFile, UnkaiError> {
    let reg_path = paths.profiles_json();
    let mut registry = if reg_path.exists() {
        load_profiles(&reg_path)?
    } else {
        ProfilesFile::default()
    };

    let mut dirty = !reg_path.exists();
    if registry.profiles.is_empty() {
        info!("Profile registry has no profiles — creating the default profile");
        registry.profiles.push(ProfileMeta::new_default());
        dirty = true;
    }

    // Legacy files always migrate into the FIRST profile: chunk 1
    // only ever has one, and on a crash-resumed run the first
    // profile is by construction the one the aborted migration
    // was targeting.
    let target_id = registry.profiles[0].id.clone();

    // Persist before moving anything — see the module docs for why
    // the id must hit disk first.
    if dirty {
        save_profiles(&reg_path, &registry)?;
    }

    let moved = migrate_legacy_files(paths, &target_id)?;
    if moved {
        info!("Migrated legacy flat storage layout into profile {target_id}");
    }
    // Theme paths are rewritten unconditionally (not just when a
    // move happened this run): a crash between the move and the
    // rewrite would otherwise leave stale absolute paths behind
    // forever.  A no-op when nothing points at the legacy dir.
    if let Err(e) = rewrite_theme_paths(paths, &target_id) {
        // Not worth failing boot over — a stale path only means a
        // custom theme doesn't load until re-imported.
        warn!("could not rewrite custom-theme paths after migration: {e}");
    }

    Ok(registry)
}

/// Record that `profile_id` is being used right now: bumps its
/// `last_used_at` and moves it to the front of the `last_used`
/// order, then persists.  Ids of since-deleted profiles are pruned
/// from the order while we're here.
pub fn touch_last_used(
    paths: &ProfilePaths,
    registry: &mut ProfilesFile,
    profile_id: &str,
) -> Result<(), UnkaiError> {
    if let Some(p) = registry.profiles.iter_mut().find(|p| p.id == profile_id) {
        p.last_used_at = Utc::now();
    }
    let known: Vec<String> = registry.profiles.iter().map(|p| p.id.clone()).collect();
    registry
        .last_used
        .retain(|id| id != profile_id && known.contains(id));
    registry.last_used.insert(0, profile_id.to_string());
    save_profiles(&paths.profiles_json(), registry)
}

/// Move every legacy flat-layout file into the profile's
/// directory.  Returns whether anything was moved.  Individual
/// moves skip a missing source (already migrated / fresh install /
/// crash-resumed run) and refuse to clobber an existing target.
fn migrate_legacy_files(paths: &ProfilePaths, profile_id: &str) -> Result<bool, UnkaiError> {
    let mut pairs: Vec<(PathBuf, PathBuf)> = vec![
        (paths.legacy_cache_db(), paths.cache_db(profile_id)),
        (paths.legacy_app_settings(), paths.app_settings(profile_id)),
        (
            paths.legacy_settings_sync(),
            paths.settings_sync(profile_id),
        ),
        (paths.legacy_themes_dir(), paths.themes_dir(profile_id)),
    ];
    // SQLite sidecars a crashed previous run may have left next to
    // the DB.  Must travel with it — a WAL replayed against nothing
    // (or worse, a future different DB at the old path) loses data.
    for suffix in ["-wal", "-shm"] {
        pairs.push((
            sidecar(&paths.legacy_cache_db(), suffix),
            sidecar(&paths.cache_db(profile_id), suffix),
        ));
    }

    if !pairs.iter().any(|(src, _)| src.exists()) {
        return Ok(false);
    }

    let dir = paths.profile_dir(profile_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| UnkaiError::Storage(format!("create profile dir {}: {e}", dir.display())))?;

    let mut moved = false;
    for (src, dst) in pairs {
        if !src.exists() {
            continue;
        }
        if dst.exists() {
            // Both existing means someone re-created a legacy file
            // after migration (or hand-copied one back).  Moving
            // over migrated data would destroy it — leave the
            // legacy file where it is and make the conflict loud.
            warn!(
                "legacy file {} and migrated {} both exist — leaving the legacy copy untouched",
                src.display(),
                dst.display()
            );
            continue;
        }
        std::fs::rename(&src, &dst).map_err(|e| {
            UnkaiError::Storage(format!(
                "migrate {} -> {}: {e}",
                src.display(),
                dst.display()
            ))
        })?;
        moved = true;
    }
    Ok(moved)
}

/// `AppSettings.custom_themes[].path` stores *absolute* paths to
/// the imported CSS files.  After the themes dir moves under the
/// profile, rewrite any stored path that still points into the
/// legacy location so the frontend keeps finding the files.
fn rewrite_theme_paths(paths: &ProfilePaths, profile_id: &str) -> Result<(), UnkaiError> {
    let file = paths.app_settings(profile_id);
    if !file.exists() {
        return Ok(());
    }
    let mut settings = crate::app_settings::load_settings(&file)?;
    let legacy_dir = paths.legacy_themes_dir();
    let new_dir = paths.themes_dir(profile_id);
    let mut changed = false;
    for theme in &mut settings.custom_themes {
        let p = PathBuf::from(&theme.path);
        if let Ok(rest) = p.strip_prefix(&legacy_dir) {
            theme.path = new_dir.join(rest).to_string_lossy().into_owned();
            changed = true;
        }
    }
    if changed {
        crate::app_settings::save_settings(&file, &settings)?;
    }
    Ok(())
}

/// `path` + `suffix` as one file name (`cache.db` → `cache.db-wal`).
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp dir removed on drop.  Deliberately built on
    /// `std::env::temp_dir()` instead of pulling in a tempdir
    /// crate — the workspace has no such dependency and the
    /// migration tests only need "a fresh dir that goes away".
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join("unkai-profiles-test")
                .join(uuid::Uuid::new_v4().to_string());
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

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, contents).expect("write fixture");
    }

    #[test]
    fn fresh_install_creates_one_default_profile() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();

        let registry = ensure_registry_files(&paths).expect("ensure");
        assert_eq!(registry.profiles.len(), 1);
        assert_eq!(registry.profiles[0].name, "Default");
        assert!(paths.profiles_json().exists());
        // Fresh install: no legacy files, so no profile dir yet —
        // the cache opener creates it lazily.
        assert!(!paths.profile_dir(&registry.profiles[0].id).exists());

        // Second boot resolves to the same profile.
        let again = ensure_registry_files(&paths).expect("ensure again");
        assert_eq!(again.profiles.len(), 1);
        assert_eq!(again.profiles[0].id, registry.profiles[0].id);
    }

    #[test]
    fn legacy_layout_migrates_into_profile_dir() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();
        write(&paths.legacy_cache_db(), "db-bytes");
        write(&sidecar(&paths.legacy_cache_db(), "-wal"), "wal-bytes");
        write(&sidecar(&paths.legacy_cache_db(), "-shm"), "shm-bytes");
        write(&paths.legacy_app_settings(), "{}");
        write(&paths.legacy_settings_sync(), "{}");
        write(&paths.legacy_themes_dir().join("storm.css"), "css");

        let registry = ensure_registry_files(&paths).expect("ensure");
        let id = &registry.profiles[0].id;

        assert!(paths.cache_db(id).exists());
        assert!(sidecar(&paths.cache_db(id), "-wal").exists());
        assert!(sidecar(&paths.cache_db(id), "-shm").exists());
        assert!(paths.app_settings(id).exists());
        assert!(paths.settings_sync(id).exists());
        assert!(paths.themes_dir(id).join("storm.css").exists());

        assert!(!paths.legacy_cache_db().exists());
        assert!(!paths.legacy_app_settings().exists());
        assert!(!paths.legacy_settings_sync().exists());
        assert!(!paths.legacy_themes_dir().exists());
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();
        write(&paths.legacy_cache_db(), "db-bytes");

        let first = ensure_registry_files(&paths).expect("first run");
        let second = ensure_registry_files(&paths).expect("second run");

        assert_eq!(first.profiles[0].id, second.profiles[0].id);
        assert_eq!(second.profiles.len(), 1);
        let db = std::fs::read_to_string(paths.cache_db(&second.profiles[0].id)).expect("read db");
        assert_eq!(db, "db-bytes");
    }

    #[test]
    fn crash_between_registry_write_and_move_resumes_same_profile() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();
        // Simulate: first run wrote profiles.json, crashed before
        // moving anything.
        let seeded = ensure_registry_files(&paths).expect("seed registry");
        let id = seeded.profiles[0].id.clone();
        // Legacy files "appear" (they were there all along, the
        // crash just never moved them).
        write(&paths.legacy_cache_db(), "db-bytes");
        write(&sidecar(&paths.legacy_cache_db(), "-wal"), "wal-bytes");

        let resumed = ensure_registry_files(&paths).expect("resume");
        assert_eq!(resumed.profiles[0].id, id);
        assert!(paths.cache_db(&id).exists());
        assert!(sidecar(&paths.cache_db(&id), "-wal").exists());
        assert!(!paths.legacy_cache_db().exists());
    }

    #[test]
    fn migration_never_clobbers_existing_profile_files() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();
        let seeded = ensure_registry_files(&paths).expect("seed");
        let id = seeded.profiles[0].id.clone();
        write(&paths.cache_db(&id), "migrated-db");
        write(&paths.legacy_cache_db(), "stray-legacy-db");

        ensure_registry_files(&paths).expect("re-run");
        let kept = std::fs::read_to_string(paths.cache_db(&id)).expect("read");
        assert_eq!(kept, "migrated-db");
        // The stray legacy file is left in place for the user to
        // inspect, not silently destroyed.
        assert!(paths.legacy_cache_db().exists());
    }

    #[test]
    fn theme_paths_are_rewritten_to_the_profile_dir() {
        let tmp = TempRoot::new();
        let paths = tmp.paths();
        let legacy_css = paths.legacy_themes_dir().join("storm.css");
        write(&legacy_css, "css");
        let settings_json = format!(
            r#"{{ "custom_themes": [ {{ "id": "storm", "label": "Storm",
                 "description": "Imported theme", "path": {} }} ] }}"#,
            serde_json::to_string(&legacy_css.to_string_lossy()).unwrap()
        );
        write(&paths.legacy_app_settings(), &settings_json);

        let registry = ensure_registry_files(&paths).expect("ensure");
        let id = &registry.profiles[0].id;
        let migrated =
            crate::app_settings::load_settings(&paths.app_settings(id)).expect("load settings");
        assert_eq!(migrated.custom_themes.len(), 1);
        let rewritten = PathBuf::from(&migrated.custom_themes[0].path);
        assert_eq!(rewritten, paths.themes_dir(id).join("storm.css"));
        assert!(rewritten.exists());
    }

    #[test]
    fn startup_profile_resolution() {
        let a = ProfileMeta {
            id: "a".into(),
            ..ProfileMeta::new_default()
        };
        let b = ProfileMeta {
            id: "b".into(),
            ..ProfileMeta::new_default()
        };
        let mut reg = ProfilesFile {
            profiles: vec![a, b],
            last_used: vec!["b".into(), "a".into()],
            ..Default::default()
        };
        assert_eq!(reg.startup_profile().unwrap().id, "b");

        reg.startup = StartupMode::Fixed("a".into());
        assert_eq!(reg.startup_profile().unwrap().id, "a");

        // A dangling fixed id falls back to the first profile.
        reg.startup = StartupMode::Fixed("gone".into());
        assert_eq!(reg.startup_profile().unwrap().id, "a");

        // Dangling last-used entries are skipped.
        reg.startup = StartupMode::LastUsed;
        reg.last_used = vec!["gone".into(), "a".into()];
        assert_eq!(reg.startup_profile().unwrap().id, "a");
    }

    #[test]
    fn registry_roundtrips_and_tolerates_missing_fields() {
        let parsed: ProfilesFile =
            serde_json::from_str(r#"{ "version": 1, "profiles": [] }"#).expect("parse partial");
        assert_eq!(parsed.startup, StartupMode::LastUsed);
        assert!(parsed.last_used.is_empty());

        let full = ProfilesFile {
            profiles: vec![ProfileMeta::new_default()],
            startup: StartupMode::Fixed("x".into()),
            last_used: vec!["x".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&full).expect("serialise");
        let back: ProfilesFile = serde_json::from_str(&json).expect("reparse");
        assert_eq!(back.startup, StartupMode::Fixed("x".into()));
        assert_eq!(back.profiles.len(), 1);
    }
}

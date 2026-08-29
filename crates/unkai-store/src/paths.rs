//! Single source of truth for where Unkai's on-disk state lives (#531).
//!
//! Before profiles, at least five modules independently built
//! `dirs::config_dir()/unkai-mail/...` paths.  Profiles turn the
//! storage layout into a *filesystem dimension* — every per-user
//! file moves under `profiles/<id>/` — so path construction has to
//! be centralised or every new consumer becomes a migration hazard.
//!
//! Target layout:
//!
//! ```text
//! <config_dir>/unkai-mail/
//! ├── profiles.json                  # profile registry (machine-global)
//! └── profiles/
//!     └── <uuid>/
//!         ├── cache.db               # existing schema, own SQLCipher key
//!         ├── app_settings.json
//!         ├── settings_sync.json
//!         └── themes/
//! ```
//!
//! The `legacy_*` accessors name the pre-profile flat layout and
//! exist only for the one-time migration in [`crate::profiles`] —
//! nothing else should ever read or write those locations.

use std::path::{Path, PathBuf};

use unkai_core::UnkaiError;

/// Resolver for every path under the app's config root.
///
/// Constructed once at boot from the OS config dir (or from an
/// explicit root in tests) and handed to whoever needs a path —
/// consumers take a `ProfilePaths` (or a concrete file path)
/// as an argument instead of computing their own.
#[derive(Debug, Clone)]
pub struct ProfilePaths {
    /// `<config_dir>/unkai-mail`
    root: PathBuf,
}

impl ProfilePaths {
    /// Resolve the standard root under the OS config directory.
    pub fn from_config_dir() -> Result<Self, UnkaiError> {
        let dir = dirs::config_dir()
            .ok_or_else(|| UnkaiError::Storage("cannot determine config directory".into()))?;
        Ok(Self {
            root: dir.join("unkai-mail"),
        })
    }

    /// Use an explicit root instead of the OS config dir.  For
    /// tests, which point this at a temp dir so the migration and
    /// registry logic never touch the user's real install.
    pub fn at_root(root: PathBuf) -> Self {
        Self { root }
    }

    /// `<config_dir>/unkai-mail`
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The machine-global profile registry file.
    pub fn profiles_json(&self) -> PathBuf {
        self.root.join("profiles.json")
    }

    /// The machine-level shared cache (#532) — plaintext SQLite
    /// holding machine-scoped data every profile reads (today: the
    /// URLhaus feed snapshot).  Deliberately at the root, next to
    /// `profiles.json`, not under any `profiles/<id>/`: no profile
    /// owns it, and deleting a profile must never touch it.
    pub fn shared_db(&self) -> PathBuf {
        self.root.join("shared.db")
    }

    /// Parent directory holding one subdirectory per profile.
    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    /// A single profile's private directory.
    pub fn profile_dir(&self, profile_id: &str) -> PathBuf {
        self.profiles_dir().join(profile_id)
    }

    /// The profile's SQLCipher-encrypted cache database.
    pub fn cache_db(&self, profile_id: &str) -> PathBuf {
        self.profile_dir(profile_id).join("cache.db")
    }

    /// The profile's app-wide preferences file.
    pub fn app_settings(&self, profile_id: &str) -> PathBuf {
        self.profile_dir(profile_id).join("app_settings.json")
    }

    /// The profile's local-only settings-sync state file.
    pub fn settings_sync(&self, profile_id: &str) -> PathBuf {
        self.profile_dir(profile_id).join("settings_sync.json")
    }

    /// The profile's user-imported themes directory.
    pub fn themes_dir(&self, profile_id: &str) -> PathBuf {
        self.profile_dir(profile_id).join("themes")
    }

    // ── Legacy (pre-profile) flat layout ───────────────────────
    //
    // Only the migration in `crate::profiles` may touch these.

    /// Where `cache.db` lived before profiles existed.
    pub fn legacy_cache_db(&self) -> PathBuf {
        self.root.join("cache.db")
    }

    /// Where `app_settings.json` lived before profiles existed.
    pub fn legacy_app_settings(&self) -> PathBuf {
        self.root.join("app_settings.json")
    }

    /// Where `settings_sync.json` lived before profiles existed.
    pub fn legacy_settings_sync(&self) -> PathBuf {
        self.root.join("settings_sync.json")
    }

    /// Where the themes directory lived before profiles existed.
    pub fn legacy_themes_dir(&self) -> PathBuf {
        self.root.join("themes")
    }
}

/// The system-font enumeration cache (#142).  Machine-global by
/// design — the installed font set is a property of the OS, not
/// of a profile — and it lives in the OS *cache* dir, not the
/// config dir, because losing it only costs one re-enumeration.
pub fn system_fonts_cache_json() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("unkai-mail").join("system_fonts.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_hangs_off_the_root() {
        let p = ProfilePaths::at_root(PathBuf::from("/tmp/unkai-test"));
        assert_eq!(
            p.profiles_json(),
            Path::new("/tmp/unkai-test/profiles.json")
        );
        assert_eq!(
            p.cache_db("abc"),
            Path::new("/tmp/unkai-test/profiles/abc/cache.db")
        );
        assert_eq!(
            p.themes_dir("abc"),
            Path::new("/tmp/unkai-test/profiles/abc/themes")
        );
        assert_eq!(p.legacy_cache_db(), Path::new("/tmp/unkai-test/cache.db"));
    }
}

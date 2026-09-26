//! In-app updater (#229) — the Rust side of the update flow.
//!
//! The whole flow is driven from here rather than through
//! `@tauri-apps/plugin-updater`'s JS API for two reasons:
//!
//!   1. **Release channels.**  The JS `check()` can only use the
//!      endpoints baked into `tauri.conf.json`; the Rust
//!      `updater_builder()` can pick endpoints at runtime, which is
//!      what lets the user's `update_channel` setting mean anything.
//!   2. **The `api/` layer contract (#473).**  Frontend components
//!      never import `@tauri-apps/*` directly, so the updater would
//!      have needed typed wrappers anyway — wrapping our own three
//!      commands is the same amount of surface with strictly more
//!      control.
//!
//! Trust model: every update bundle is signed in CI with the
//! project's minisign private key (`TAURI_SIGNING_PRIVATE_KEY`
//! repo secret); the plugin verifies the signature against the
//! public key in `tauri.conf.json` before anything is installed.
//! A manifest that doesn't verify is treated as "no update".
//!
//! State machine, mirrored by the frontend store:
//!
//!   check_for_update  →  found: the `Update` handle parks in
//!                        `PendingUpdate` (machine-global managed
//!                        state — updates are per-install, not
//!                        per-profile)
//!   download_update   →  streams the bundle, emitting
//!                        `update-download-progress` events to the
//!                        invoking window; bytes park next to the
//!                        handle
//!   install_update    →  verifies + installs the parked bytes,
//!                        then restarts.  On Windows the installer
//!                        exits the process itself, so the restart
//!                        call is only reached on macOS/Linux.
//!
//! Install kinds (#601): the updater only *owns* the install on
//! Windows, macOS and the Linux AppImage.  A `.deb` / `.rpm` /
//! pacman (AUR) / Flatpak install is the distro package manager's
//! property — the plugin's Linux install path writes to the file
//! named by `$APPIMAGE`, which such installs don't have, and a
//! "successful" self-update on a pacman-owned binary would desync
//! the package database.  So on a *managed* install the check still
//! runs (badge + "vX is available" keep working) but nothing is
//! parked, and download / install refuse outright.  [`InstallKind`]
//! is the one source of truth; the frontend reads it via
//! `get_install_kind` and adapts the Updates page.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Window};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Mutex;
use unkai_core::UnkaiError;

/// Stable-channel manifest — attached to the newest *published*
/// (non-draft, non-prerelease) GitHub Release by the release
/// workflow.  GitHub's `/releases/latest/` redirect is what makes
/// this a rolling URL.
const STABLE_MANIFEST_URL: &str =
    "https://github.com/firn-labs/unkai-mail/releases/latest/download/latest.json";

/// Beta-channel manifest — a rolling `beta`-tagged pre-release we
/// don't publish yet.  Listed FIRST for the beta channel so beta
/// users get pre-release builds the moment we start shipping them;
/// until then the fetch 404s and the updater falls through to the
/// stable URL, so picking Beta today simply follows stable (the
/// settings UI says so).
const BETA_MANIFEST_URL: &str =
    "https://github.com/firn-labs/unkai-mail/releases/download/beta/latest.json";

/// The update found by the last successful check, parked between
/// commands (the `Update` handle carries the download URL +
/// signature; the `Vec<u8>` appears once `download_update` ran).
/// Machine-global managed state: an update applies to the install,
/// not to a profile, so this deliberately lives OUTSIDE the
/// `ProfileRegistry` — and a check kicked off from one profile
/// window is visible to every window.
#[derive(Default)]
pub struct PendingUpdate(pub Mutex<Option<(Update, Option<Vec<u8>>)>>);

/// How this process was installed — decides whether the in-app
/// updater may touch the install at all (#601).  Serialised in
/// kebab-case for the `get_install_kind` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallKind {
    /// Windows (NSIS / MSI) or macOS (.app) — the updater plugin
    /// owns these installs.
    Native,
    /// Linux AppImage — self-contained, the plugin swaps the file
    /// named by `$APPIMAGE`.
    AppImage,
    /// Linux `.deb` / `.rpm` / pacman (AUR) / any other distro
    /// package — updates belong to the package manager.
    Package,
    /// Flatpak sandbox — updates belong to `flatpak update`.
    Flatpak,
}

impl InstallKind {
    /// Whether the in-app updater may download + install here.
    pub fn self_update(self) -> bool {
        matches!(self, InstallKind::Native | InstallKind::AppImage)
    }

    /// Pure mapping over the environment facts, so the rule is unit
    /// testable without faking `std::env`.  `flatpak_info` is
    /// "`/.flatpak-info` exists" — Flatpak always mounts it, even
    /// when `FLATPAK_ID` was scrubbed from the environment.
    fn classify(linux: bool, appimage: bool, flatpak_id: bool, flatpak_info: bool) -> Self {
        if !linux {
            InstallKind::Native
        } else if flatpak_id || flatpak_info {
            InstallKind::Flatpak
        } else if appimage {
            InstallKind::AppImage
        } else {
            InstallKind::Package
        }
    }

    /// Detect the running process's install kind.  Cheap (two env
    /// reads + one stat), so it isn't cached — and the env can't
    /// change under a running process anyway.
    pub fn detect() -> Self {
        let has = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        Self::classify(
            cfg!(target_os = "linux"),
            has("APPIMAGE"),
            has("FLATPAK_ID"),
            std::path::Path::new("/.flatpak-info").exists(),
        )
    }
}

/// The error every managed-install refusal carries — one string so
/// the frontend can match it if it ever needs to.
fn managed_install_error(kind: InstallKind) -> UnkaiError {
    UnkaiError::Other(format!(
        "this install is managed by the system package manager ({kind:?}) — update it there"
    ))
}

/// What `check_for_update` reports back to the UI.  `version` etc.
/// are `None` when the app is already current.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub available: bool,
    pub current_version: String,
    pub version: Option<String>,
    /// Release notes from the manifest (tauri-action copies the
    /// GitHub Release body in).  Plain text/markdown as-is; the
    /// frontend renders it verbatim.
    pub notes: Option<String>,
    /// Publish date as a Unix timestamp (seconds) — the frontend
    /// localises it; `time`'s own formatting stays out of the IPC.
    pub date: Option<i64>,
}

/// Progress payload for the `update-download-progress` event.
/// `total` is `None` when the server didn't send Content-Length.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    downloaded: u64,
    total: Option<u64>,
}

fn manifest_urls(channel: &str) -> Vec<&'static str> {
    match channel {
        // Beta first, stable as the fallback — see BETA_MANIFEST_URL.
        "beta" => vec![BETA_MANIFEST_URL, STABLE_MANIFEST_URL],
        // Unknown strings (a settings file synced back from a
        // future version with more channels) degrade to stable
        // rather than erroring.
        _ => vec![STABLE_MANIFEST_URL],
    }
}

/// Query the channel's manifest for a newer version.  A found
/// update replaces whatever was parked — UNLESS it is the same
/// version that's already parked, in which case the park (and any
/// already-downloaded bytes) survives: checks run on per-window
/// schedules (startup + 6-hourly, in every profile window) against
/// this one machine-global park, so a background re-check routinely
/// lands between "download finished" and the user's "Restart now"
/// click, and wiping the stash there strands the install with
/// "update not downloaded yet" (#566).  The frontend store applies
/// the same rule (same version keeps `downloaded`), so the two
/// state machines stay in step.  "No update" still clears the park
/// so a stale badge can't survive a release being pulled.
pub async fn check_for_update(
    app: &AppHandle,
    channel: &str,
    pending: &PendingUpdate,
) -> Result<UpdateCheckResult, UnkaiError> {
    let urls = manifest_urls(channel)
        .into_iter()
        .map(|u| u.parse().expect("static manifest URL parses"))
        .collect();
    let updater = app
        .updater_builder()
        .endpoints(urls)
        .map_err(|e| UnkaiError::Other(format!("updater endpoints rejected: {e}")))?
        .build()
        .map_err(|e| UnkaiError::Other(format!("updater init failed: {e}")))?;

    let found = updater
        .check()
        .await
        .map_err(|e| UnkaiError::Network(format!("update check failed: {e}")))?;

    let current_version = app.package_info().version.to_string();
    let mut slot = pending.0.lock().await;
    match found {
        Some(update) => {
            let result = UpdateCheckResult {
                available: true,
                current_version,
                version: Some(update.version.clone()),
                notes: update.body.clone(),
                date: update.date.map(|d| d.unix_timestamp()),
            };
            // Managed install (#601): report it, never park it —
            // the park is what download / install act on, and a
            // package-manager-owned binary is not ours to replace.
            if !InstallKind::detect().self_update() {
                *slot = None;
                return Ok(result);
            }
            match slot.take() {
                // Same version already parked: keep it, bytes and
                // all — see the doc comment above.
                Some(parked) if parked.0.version == update.version => *slot = Some(parked),
                _ => *slot = Some((update, None)),
            }
            Ok(result)
        }
        None => {
            *slot = None;
            Ok(UpdateCheckResult {
                available: false,
                current_version,
                version: None,
                notes: None,
                date: None,
            })
        }
    }
}

/// Download the parked update's bundle, streaming progress to the
/// invoking window as `update-download-progress` events.  The bytes
/// park in memory next to the handle — deliberately not on disk, so
/// there's no partially-written installer to clean up and nothing
/// to re-verify beyond the plugin's own signature check at install
/// time.  Idempotent: re-invoking after a completed download is a
/// no-op, so the UI can't double-fetch.
pub async fn download_update(window: &Window, pending: &PendingUpdate) -> Result<(), UnkaiError> {
    let kind = InstallKind::detect();
    if !kind.self_update() {
        return Err(managed_install_error(kind));
    }
    // Clone the handle out instead of holding the lock across the
    // whole download — a concurrent `check_for_update` from another
    // window must not deadlock behind a multi-minute fetch.
    let update = {
        let slot = pending.0.lock().await;
        match slot.as_ref() {
            Some((_, Some(_))) => return Ok(()), // already downloaded
            Some((update, None)) => update.clone(),
            None => {
                return Err(UnkaiError::Other(
                    "no pending update — run a check first".to_string(),
                ));
            }
        }
    };

    let label = window.label().to_string();
    let win = window.clone();
    let mut downloaded: u64 = 0;
    let bytes = update
        .download(
            move |chunk, total| {
                downloaded += chunk as u64;
                // Window-targeted (#535): only the surface that asked
                // for the download renders a progress bar.
                if let Err(e) = win.emit_to(
                    label.as_str(),
                    "update-download-progress",
                    DownloadProgress { downloaded, total },
                ) {
                    tracing::warn!("update progress emit failed: {e}");
                }
            },
            || {},
        )
        .await
        .map_err(|e| UnkaiError::Network(format!("update download failed: {e}")))?;

    let mut slot = pending.0.lock().await;
    match slot.as_mut() {
        // Guard against a check() having swapped the park while we
        // were downloading — don't marry old bytes to a new handle.
        Some((parked, stash)) if parked.version == update.version => *stash = Some(bytes),
        _ => {
            return Err(UnkaiError::Other(
                "pending update changed during download — check again".to_string(),
            ));
        }
    }
    Ok(())
}

/// Verify + install the downloaded bundle, then restart into the
/// new version.  On Windows the NSIS installer takes over and exits
/// this process itself; on macOS/Linux the explicit `restart()`
/// does it.  Either way the caller never sees a return on success —
/// the frontend treats this as fire-and-forget after its own
/// "restart now?" confirmation.
pub async fn install_update(app: &AppHandle, pending: &PendingUpdate) -> Result<(), UnkaiError> {
    let kind = InstallKind::detect();
    if !kind.self_update() {
        return Err(managed_install_error(kind));
    }
    let (update, bytes) = {
        let mut slot = pending.0.lock().await;
        match slot.take() {
            Some((update, Some(bytes))) => (update, bytes),
            Some(parked) => {
                // Put the undownloaded handle back — the check
                // result is still valid, the UI just skipped a step.
                *slot = Some(parked);
                return Err(UnkaiError::Other("update not downloaded yet".to_string()));
            }
            None => {
                return Err(UnkaiError::Other(
                    "no pending update — run a check first".to_string(),
                ));
            }
        }
    };

    if let Err(e) = update.install(bytes) {
        // The bytes are gone (install consumed them), but re-parking
        // the handle lets the UI recover with a plain re-download
        // instead of demanding a fresh check first (#566).
        *pending.0.lock().await = Some((update, None));
        return Err(UnkaiError::Other(format!("update install failed: {e}")));
    }
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::InstallKind;

    #[test]
    fn non_linux_is_always_native() {
        // Whatever env leaks in (an AppImage var set by a shell
        // profile, say), Windows / macOS installs are the plugin's.
        assert_eq!(
            InstallKind::classify(false, true, true, true),
            InstallKind::Native
        );
        assert_eq!(
            InstallKind::classify(false, false, false, false),
            InstallKind::Native
        );
    }

    #[test]
    fn linux_appimage_self_updates() {
        let k = InstallKind::classify(true, true, false, false);
        assert_eq!(k, InstallKind::AppImage);
        assert!(k.self_update());
    }

    #[test]
    fn linux_without_appimage_is_a_managed_package() {
        let k = InstallKind::classify(true, false, false, false);
        assert_eq!(k, InstallKind::Package);
        assert!(!k.self_update());
    }

    #[test]
    fn flatpak_wins_over_appimage_and_is_managed() {
        // Either Flatpak signal is enough on its own.
        assert_eq!(
            InstallKind::classify(true, false, true, false),
            InstallKind::Flatpak
        );
        assert_eq!(
            InstallKind::classify(true, false, false, true),
            InstallKind::Flatpak
        );
        // A stray APPIMAGE inside the sandbox doesn't unmanage it.
        let k = InstallKind::classify(true, true, true, false);
        assert_eq!(k, InstallKind::Flatpak);
        assert!(!k.self_update());
    }

    #[test]
    fn serialises_kebab_case_for_the_frontend() {
        assert_eq!(
            serde_json::to_string(&InstallKind::AppImage).unwrap(),
            "\"app-image\""
        );
        assert_eq!(
            serde_json::to_string(&InstallKind::Package).unwrap(),
            "\"package\""
        );
    }
}

//! Profile-window management (#535).
//!
//! One process, one *primary* window per profile: the statically
//! declared `"main"` window hosts the startup profile, and every
//! additional profile lives in a dynamically created `profile-*`
//! window.  This module is the only place that creates, finds, or
//! raises those windows — every "bring the app to the front"
//! surface (tray, single-instance second launch, mailto deep
//! links, notification clicks, the `show_main_window` IPC command)
//! funnels through [`show_primary_window`] instead of hardcoding
//! `get_webview_window("main")`.
//!
//! Popout windows (compose-*, mail-*, …) are created by the
//! frontend's shared popout helper; they register their label →
//! profile mapping via the `register_popout_window` command before
//! creation and are never treated as a profile's home window.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use unkai_core::UnkaiError;
use unkai_store::ProfilePaths;

use crate::registry::{self, ProfileRegistry};
use crate::tray::logo_bytes_for;

/// True for labels that host the full app shell for a profile —
/// the static `"main"` window or a dynamically created
/// `profile-*` window.  Popouts also map to a profile in the
/// registry but are never a profile's "home" window.
pub fn is_primary_label(label: &str) -> bool {
    label == "main" || label.starts_with("profile-")
}

/// Show + unminimize + focus, with the #470 Linux decoration
/// repair every un-hide needs.
fn raise(win: &WebviewWindow) {
    // show() may be a no-op if the window is already visible, but
    // unminimize() + set_focus() still make sense in that case.
    let _ = win.show();
    let _ = win.unminimize();
    let _ = win.set_focus();
    crate::rebuild_decoration_input_region(win);
}

/// The profile's currently live primary window, if it has one.
fn primary_window_for_profile(app: &AppHandle, profile_id: &str) -> Option<WebviewWindow> {
    let reg = app.state::<ProfileRegistry>();
    reg.labels_for_profile(profile_id)
        .into_iter()
        .filter(|label| is_primary_label(label))
        .find_map(|label| app.get_webview_window(&label))
}

/// Make sure the profile's runtime context is open, building and
/// inserting one when it isn't (a profile whose windows were all
/// closed keeps its context — this only rebuilds after a
/// `delete_profile`-style shutdown or for a never-opened profile).
pub fn ensure_profile_context(app: &AppHandle, profile_id: &str) -> Result<(), UnkaiError> {
    let reg = app.state::<ProfileRegistry>();
    if reg.context_for(profile_id).is_some() {
        return Ok(());
    }
    // Validate against the registry file before opening caches —
    // building a handle creates the profile's directory tree, and
    // doing that for a typo'd id would fabricate a ghost profile.
    let known = unkai_store::profiles::load_profiles(&reg.paths().profiles_json())?;
    if !known.profiles.iter().any(|p| p.id == profile_id) {
        return Err(UnkaiError::Other(format!(
            "no profile with id '{profile_id}'"
        )));
    }
    let handle = registry::build_profile_handle(app, profile_id, reg.paths())?;
    reg.insert_profile(profile_id, handle);
    Ok(())
}

/// A `profile-*` label that is not currently in use.  Normally
/// `profile-<id>`; the suffixed fallback covers the corner where a
/// switch-in-place left a window *labelled* after this profile but
/// *showing* another one — labels are immutable, mappings aren't.
fn free_profile_label(app: &AppHandle, profile_id: &str) -> String {
    let base = format!("profile-{profile_id}");
    if app.get_webview_window(&base).is_none() {
        return base;
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}-{n}");
        if app.get_webview_window(&candidate).is_none() {
            return candidate;
        }
        n += 1;
    }
}

/// Create a primary window for the profile.  The label is mapped
/// in the registry BEFORE the webview exists so its very first IPC
/// command already resolves to the right profile.  Mirrors the
/// static `"main"` window's shape from `tauri.conf.json`,
/// including the created-hidden dance: the icon goes on first so
/// the taskbar never paints a frame with the wrong logo style.
///
/// `focus` steals the foreground via [`raise`] — required for
/// user-initiated raises (a plain `show()` on a created-hidden
/// window never runs the platform foreground-lock workarounds, so
/// the window would come up BEHIND the app the user was in on
/// Windows/Linux).  The startup fan-out passes `false` so N boot
/// windows don't fight over focus.
pub fn create_profile_window(
    app: &AppHandle,
    profile_id: &str,
    show: bool,
    focus: bool,
) -> Result<WebviewWindow, UnkaiError> {
    let reg = app.state::<ProfileRegistry>();
    let label = free_profile_label(app, profile_id);
    reg.map_window(&label, profile_id);

    let url = format!("index.html?profile={profile_id}");
    let built = WebviewWindowBuilder::new(app, &label, WebviewUrl::App(url.into()))
        .title("Unkai Mail")
        .inner_size(1200.0, 800.0)
        .resizable(true)
        .visible(false)
        .disable_drag_drop_handler()
        .build();
    let win = match built {
        Ok(win) => win,
        Err(e) => {
            // Never leave a mapping for a window that won't exist.
            reg.unmap_window(&label);
            return Err(UnkaiError::Other(format!(
                "could not create window for profile '{profile_id}': {e}"
            )));
        }
    };

    // The window icon follows the profile's own logo style.
    // `try_read` instead of a blocking read: this runs from async
    // commands and the setup hook alike, and an (unlikely)
    // contended lock only costs us the default style for this
    // window's lifetime.
    let style = reg
        .context_for(profile_id)
        .and_then(|h| h.ctx.settings.try_read().map(|s| s.logo_style.clone()).ok())
        .unwrap_or_else(|| "storm".to_string());
    if let Ok(img) = tauri::image::Image::from_bytes(logo_bytes_for(&style))
        && let Err(e) = win.set_icon(img)
    {
        tracing::warn!("failed to apply window icon on profile window: {e}");
    }

    if show {
        if focus {
            // show + unminimize + set_focus + the #470 repair.
            raise(&win);
        } else {
            let _ = win.show();
            // #470 — the `visible: false` → `show()` transition is
            // where the Linux titlebar loses its buttons.
            crate::rebuild_decoration_input_region(&win);
        }
    }
    Ok(win)
}

/// Focus the profile's primary window, creating one (and its
/// runtime context) when none exists.
pub fn focus_or_create_profile_window(app: &AppHandle, profile_id: &str) -> Result<(), UnkaiError> {
    if let Some(win) = primary_window_for_profile(app, profile_id) {
        raise(&win);
        return Ok(());
    }
    ensure_profile_context(app, profile_id)?;
    create_profile_window(app, profile_id, true, true).map(|_| ())
}

/// The primary window [`show_primary_window`] raised, and whether
/// it had to be created fresh — a fresh webview has no event
/// listeners yet, so callers wanting to hand it an event must use
/// the cold-start buffers instead of emitting.
pub struct RaisedWindow {
    pub window: WebviewWindow,
    pub created: bool,
}

/// Bring "the app" to the front when the caller has no profile of
/// its own: the most recently focused primary window, else any
/// live primary window, else a (re)created window for the startup
/// profile.  Returns the raised window so callers can target
/// events at it (tray Compose, mailto forwarding).
pub fn show_primary_window(app: &AppHandle) -> Result<RaisedWindow, UnkaiError> {
    let reg = app.state::<ProfileRegistry>();
    if let Some(label) = reg.last_focused_label()
        && let Some(win) = app.get_webview_window(&label)
    {
        raise(&win);
        return Ok(RaisedWindow {
            window: win,
            created: false,
        });
    }
    // No focus bookmark (e.g. booted straight into the tray with
    // start_minimized) — fall back to any live primary window,
    // preferring the startup profile's.
    let startup = reg.startup_profile_id().to_string();
    if let Some(win) = primary_window_for_profile(app, &startup) {
        raise(&win);
        return Ok(RaisedWindow {
            window: win,
            created: false,
        });
    }
    for id in reg.profiles_with_windows() {
        if let Some(win) = primary_window_for_profile(app, &id) {
            raise(&win);
            return Ok(RaisedWindow {
                window: win,
                created: false,
            });
        }
    }
    ensure_profile_context(app, &startup)?;
    create_profile_window(app, &startup, true, true).map(|window| RaisedWindow {
        window,
        created: true,
    })
}

/// Persist the `last_used` bookkeeping for a profile off the event
/// loop — a tiny JSON rewrite, but window-focus events fire on the
/// UI thread and file IO does not belong there.  Runs through
/// `update_registry`, which serialises against the profile CRUD
/// commands — an unsynchronised save here could clobber a profile
/// the user just created in Settings.
pub fn persist_last_used(paths: ProfilePaths, profile_id: String) {
    tauri::async_runtime::spawn_blocking(move || {
        let result = unkai_store::profiles::update_registry(&paths, |registry| {
            unkai_store::profiles::touch_last_used_entry(registry, &profile_id);
            Ok(())
        });
        if let Err(e) = result {
            tracing::warn!("could not update profile last-used bookkeeping: {e}");
        }
    });
}

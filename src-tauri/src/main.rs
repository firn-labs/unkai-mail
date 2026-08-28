//! Unkai — a modern mail client with Nextcloud integration.
//!
//! This is the Tauri **desktop shell** (#476).  The application layer
//! — every command body, the background loops, the crypto bridge —
//! lives in the transport-agnostic `unkai-commands` crate; what
//! remains here is:
//!
//!   * one thin `#[tauri::command]` shim per command, which extracts
//!     the managed state and delegates,
//!   * the desktop chrome: tray, menus, windows, native notifications,
//!     deep links, launch-argv capture, URI-scheme protocols,
//!   * [`notifier::TauriNotifier`] — the desktop implementation of
//!     `unkai_commands::UiNotifier`, the seam the application layer
//!     talks back to the UI through.
//!
//! Keep it that way: new command logic goes in `unkai-commands`
//! (in the module mirroring the frontend's `ui/src/lib/api/` domain),
//! and only its shim is added here.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod badge;
mod notifier;
mod registry;
mod tray;
mod windows;

use std::sync::{Arc, Mutex};

use registry::{ProfileRegistry, profile_ctx};
use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, UriSchemeContext, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::RwLock;
use tray::{TrayBaseIcon, decode_logo_png, logo_assets, logo_bytes_for};
use unkai_commands as cmds;
use unkai_commands::mail::refresh_unread_badge;
use unkai_commands::system::{
    FontCacheFile, compute_font_fingerprint, enumerate_system_fonts, load_font_cache_file,
    save_font_cache_file,
};
use unkai_core::UnkaiError;
use unkai_core::models::{
    Account, AppSettings, CalendarEvent, Contact, CustomTheme, Email, EmailEnvelope, Folder,
    NextcloudAccount, OutgoingEmail, Task, TaskList,
};
use unkai_mcp::McpServerStatus;
use unkai_nextcloud::{FileEntry, LoginFlowInit};
use unkai_store::cache::{SearchFilters, SearchHit, SearchScope};
use unkai_store::profiles::{ProfileIcon, ProfileMeta, StartupMode};
use unkai_store::{credentials, link_check};

use unkai_commands::accounts::ProbedCert;
use unkai_commands::calendar::{
    AttendeeAvailability, CalendarEventInput, CalendarSummary, ImportCalendarReport, InviteSummary,
    NextcloudMapsCapability, SyncCalendarsReport,
};
use unkai_commands::compose::{
    DraftReplaceSource, OutboxRowDto, OutboxSourceRef, RepliedToRef, SavedDraft,
};
use unkai_commands::contacts::{
    AddressbookSummary, ContactCategoryView, ContactGroupView, ContactInput, ContactPhoto,
    ImportContactsReport, MailingListView, SyncContactsReport,
};
use unkai_commands::crypto::{PgpKeyStatus, PgpPublicKeyDto, SmimeCertDto, SmimeCertStatus};
use unkai_commands::mail::{AttachmentPreviewView, InlineImageView, LinkVerdict};
use unkai_commands::nextcloud::{
    NextcloudGroupView, NextcloudShareResult, NextcloudShareRow, NextcloudUserLookup,
};
use unkai_commands::settings::{
    DatabaseStatusView, FidoStatusView, McpToolView, SettingsSyncStateView, WipePolicyView,
};
use unkai_commands::state::SystemFontsCache;
use unkai_commands::support::SyncStatus;
use unkai_commands::system::{OfficeOpenResult, PdfOpenResult};

/// Absolute filesystem path to a PNG of our app icon, written at
/// startup. Returned to the frontend via `get_notification_icon_path`
/// so `sendNotification` calls can pass it through to libnotify /
/// the Windows toast / NSUserNotification on macOS, ensuring our
/// own icon shows up in the toast instead of a generic placeholder
/// (especially in dev builds where no .desktop / Start-Menu shortcut
/// exists yet to lend the OS a registered icon).
struct NotificationIconPath(std::path::PathBuf);

/// Bytes of `icons/icon.png`, baked in at compile time so we can
/// drop them onto disk on first launch without having to resolve a
/// runtime resource path that differs between `cargo tauri dev` and
/// bundled builds.
const NOTIFICATION_ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

/// Write the embedded icon to a stable temp-dir path and return it.
/// Idempotent — overwriting on every launch is cheap (~10 KB) and
/// keeps the file in sync with whatever's currently bundled.
fn install_notification_icon() -> Result<std::path::PathBuf, UnkaiError> {
    // The file is the bundled app icon (a static asset, identical
    // for all installs).  Predictable name in the per-user temp
    // dir is intentional — Windows' notification API needs a
    // stable on-disk path to reference.  No secret data here.
    // nosemgrep: rust.lang.security.temp-dir.temp-dir
    let dir = std::env::temp_dir().join("unkai-mail");
    std::fs::create_dir_all(&dir)
        .map_err(|e| UnkaiError::Other(format!("notification icon mkdir failed: {e}")))?;
    let path = dir.join("unkai-mail-icon.png");
    std::fs::write(&path, NOTIFICATION_ICON_PNG)
        .map_err(|e| UnkaiError::Other(format!("notification icon write failed: {e}")))?;
    Ok(path)
}

#[tauri::command]
fn get_notification_icon_path(state: State<'_, NotificationIconPath>) -> String {
    state.0.to_string_lossy().into_owned()
}

/// `notification-clicked` event payload (#415): the identity
/// triple of the message a clicked notification refers to.  The
/// frontend routes it through the same in-view open path the Notes
/// `mail://` deep-link uses.  Window focus happens on the Rust
/// side before the event is emitted, because JS `setFocus()` from
/// a background window is unreliable on Windows
/// (`SetForegroundWindow` lock).  Since #535 the payload also
/// carries the owning profile, so the click focuses *that*
/// profile's window and only its shell handles the deep link.
#[cfg(any(target_os = "linux", windows))]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationClickPayload {
    account_id: String,
    folder: String,
    uid: u32,
    profile_id: String,
}

/// Assemble the optional deep-link target from the three optional
/// IPC args (#415).  All-or-nothing: a notification either
/// references a concrete message (new-mail and reminder toasts) or
/// it's informational (burst summaries) and clicking it does
/// nothing.
#[cfg(any(target_os = "linux", windows))]
fn notification_click_target(
    account_id: Option<String>,
    folder: Option<String>,
    uid: Option<u32>,
    profile_id: String,
) -> Option<NotificationClickPayload> {
    match (account_id, folder, uid) {
        (Some(account_id), Some(folder), Some(uid)) => Some(NotificationClickPayload {
            account_id,
            folder,
            uid,
            profile_id,
        }),
        _ => None,
    }
}

/// Focus the owning profile's window and tell it which message the
/// clicked notification referred to (#415/#535).  Shared by the
/// Linux action handler and the Windows toast-activation callback.
#[cfg(any(target_os = "linux", windows))]
fn handle_notification_click(app: &AppHandle, payload: &NotificationClickPayload) {
    if let Err(e) = windows::focus_or_create_profile_window(app, &payload.profile_id) {
        tracing::warn!("notification-click window raise failed: {e}");
    }
    // Target the profile's primary windows only — with several
    // profiles open, a broadcast would deep-link every shell at
    // once.  If the window had to be re-created above, the fresh
    // webview may miss this emit (no listener yet); the click then
    // just focuses the window, which is the pre-#415 behaviour.
    let reg = app.state::<ProfileRegistry>();
    for label in reg.labels_for_profile(&payload.profile_id) {
        if windows::is_primary_label(&label)
            && let Err(e) = app.emit_to(label.as_str(), "notification-clicked", payload)
        {
            tracing::warn!("failed to emit notification-clicked event: {e}");
        }
    }
}

/// Linux: send a desktop notification through libnotify with the
/// `DesktopEntry` + `Category` hints set, so the notification
/// daemon (GNOME Shell / KDE Plasma / mako / dunst) tracks it under
/// our app identity and keeps it in its notification center.
///
/// `tauri-plugin-notification` uses notify-rust under the hood but
/// doesn't expose hint APIs in JS, which left dev-build toasts as
/// "anonymous" — they showed up briefly but weren't kept in the
/// notification history. Wrapping the builder ourselves with the
/// hints set is enough to make them persist.
///
/// #415: when the caller identifies a message (`account_id` +
/// `folder` + `uid`), the notification carries a default action
/// and a detached thread waits for the daemon's click callback —
/// clicking the toast then focuses the main window and deep-links
/// to that message via the `notification-clicked` event.
///
/// Returns `Ok(true)` when the call succeeded so the JS side can
/// fall back to the regular plugin if anything goes wrong (e.g.
/// no notification daemon running).
#[cfg(target_os = "linux")]
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri shim: params mirror the IPC payload + managed state
fn send_native_notification(
    app: AppHandle,
    title: String,
    body: String,
    account_id: Option<String>,
    folder: Option<String>,
    uid: Option<u32>,
    icon: State<'_, NotificationIconPath>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<bool, UnkaiError> {
    use notify_rust::{Hint, Notification};
    let target = notification_click_target(
        account_id,
        folder,
        uid,
        reg.profile_for_label(window.label()),
    );
    let mut n = Notification::new();
    n.summary(&title)
        .body(&body)
        .appname("Unkai Mail")
        .hint(Hint::DesktopEntry("com.unkai.mail".to_string()))
        .hint(Hint::Category("email".to_string()));
    let icon_path = icon.0.to_string_lossy();
    if !icon_path.is_empty() {
        n.icon(&icon_path);
    }
    if target.is_some() {
        // "default" is the reserved XDG action id daemons fire when
        // the user clicks the notification body itself — no visible
        // button is rendered for it.  Daemons without action support
        // simply never invoke it; the toast still shows.
        n.action("default", "Open");
    }
    let handle = n
        .show()
        .map_err(|e| UnkaiError::Other(format!("notify-rust failed: {e}")))?;
    if let Some(payload) = target {
        // `wait_for_action` parks until the notification is
        // activated or closed — one short-lived OS thread per live
        // toast is fine at desktop notification volumes.
        std::thread::spawn(move || {
            handle.wait_for_action(move |action| {
                if action == "default" {
                    handle_notification_click(&app, &payload);
                }
            });
        });
    }
    Ok(true)
}

/// Windows: send the toast through WinRT directly (the same
/// backend the notification plugin uses underneath) so we can
/// attach an activation callback — the plugin exposes no desktop
/// click event, and without the callback a clicked toast does
/// nothing (#415).  The explicit AUMID matches the one
/// `set_app_user_model_id` registers at process startup, so toasts
/// attribute to "Unkai Mail" either way.
///
/// Returns `Ok(false)` when the WinRT call fails so the JS side
/// falls back to the plugin — the toast still shows, only the
/// click deep-link is lost.
#[cfg(windows)]
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri shim: params mirror the IPC payload + managed state
fn send_native_notification(
    app: AppHandle,
    title: String,
    body: String,
    account_id: Option<String>,
    folder: Option<String>,
    uid: Option<u32>,
    icon: State<'_, NotificationIconPath>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<bool, UnkaiError> {
    use tauri_winrt_notification::{IconCrop, Toast};

    let target = notification_click_target(
        account_id,
        folder,
        uid,
        reg.profile_for_label(window.label()),
    );
    let mut toast = Toast::new("com.unkai.mail").title(&title).text1(&body);
    if !icon.0.as_os_str().is_empty() {
        toast = toast.icon(&icon.0, IconCrop::Square, "Unkai Mail");
    }
    if let Some(payload) = target {
        toast = toast.on_activated(move |_action| {
            handle_notification_click(&app, &payload);
            Ok(())
        });
    }
    match toast.show() {
        Ok(()) => Ok(true),
        Err(e) => {
            tracing::warn!("winrt toast failed, falling back to the plugin: {e}");
            Ok(false)
        }
    }
}

/// Stub on the remaining desktop platform (macOS) — the JS side
/// falls back to `sendNotification` from the Tauri plugin when
/// this returns `Ok(false)`.  Click deep-linking (#415) isn't
/// wired there yet: the plugin exposes no desktop click event and
/// the OS-level notification delegate it would take is a
/// follow-up.  The unused message args keep the IPC payload shape
/// identical across platforms.
#[cfg(not(any(target_os = "linux", windows)))]
#[tauri::command]
fn send_native_notification(
    _title: String,
    _body: String,
    _account_id: Option<String>,
    _folder: Option<String>,
    _uid: Option<u32>,
) -> Result<bool, UnkaiError> {
    Ok(false)
}

/// Tells Windows that this process should attribute its toast
/// notifications to a specific AUMID instead of inheriting the
/// launching process's (which surfaces as "PowerShell" / "cmd" /
/// "Git Bash" depending on how the dev binary was started).
///
/// The string MUST match the AUMID baked into the installer's
/// Start-Menu shortcut for the toast's display name + icon to
/// resolve correctly in installed builds; we use the same bundle
/// identifier (`com.unkai.mail`) the Tauri config sets so the two
/// stay in lockstep.
#[cfg(windows)]
fn set_app_user_model_id() {
    // `::windows` = the windows-rs crate — the leading `::`
    // disambiguates from our own `crate::windows` module (#535).
    use ::windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use ::windows::core::HSTRING;

    let aumid = HSTRING::from("com.unkai.mail");
    // SAFETY: the function takes a PCWSTR derived from a live
    // HSTRING; the call has no preconditions beyond a valid
    // null-terminated wide string, which `HSTRING` guarantees.
    if let Err(e) =
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe { SetCurrentProcessExplicitAppUserModelID(&aumid) }
    {
        tracing::warn!("SetCurrentProcessExplicitAppUserModelID failed: {e}");
    }
}

// ── Custom URI scheme: contact photos ──────────────────────────
//
// Contact avatars are served via a custom `contact-photo://<id>`
// scheme so the webview can request them with a plain `<img src>`
// instead of round-tripping the bytes through the JSON IPC layer.
// JSON serialises a byte as one number (3–4 chars per byte), so
// shipping 200 photos that way turned the contacts list into tens
// of MB of IPC traffic. Going through a URI scheme:
//
// - the body is raw bytes — no encoding bloat
// - the browser caches per-URL, so scrolling a row off and back on
//   doesn't re-fetch
// - `loading="lazy"` on the `<img>` defers fetches for off-screen
//   rows, so opening a 1000-contact addressbook only pays for the
//   ~20 photos actually visible
//
// The path component of the URL is the contact's app-side id,
// percent-encoded by `convertFileSrc` on the JS side.
fn contact_photo_protocol(
    ctx: UriSchemeContext<'_, tauri::Wry>,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<std::borrow::Cow<'static, [u8]>> {
    let id = percent_decode(request.uri().path().trim_start_matches('/'));
    // Resolve the requesting webview's profile (#533): avatars come
    // out of that profile's cache, same as every command.
    let reg = ctx.app_handle().state::<ProfileRegistry>();
    let handle = match reg.handle_for_label(ctx.webview_label()) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("contact-photo request from unresolvable webview: {e}");
            return tauri::http::Response::builder()
                .status(500)
                .body(std::borrow::Cow::Owned(Vec::new()))
                .expect("build 500");
        }
    };
    let cache = &handle.ctx.cache;
    match cache.get_contact_photo(&id) {
        Ok(Some((mime, bytes))) => tauri::http::Response::builder()
            .status(200)
            .header("Content-Type", mime)
            // The bytes are immutable per (id, etag) — but we don't
            // know the etag here. A short cache window is enough to
            // dedupe the burst of requests that comes from scrolling.
            .header("Cache-Control", "private, max-age=300")
            .body(std::borrow::Cow::Owned(bytes))
            .expect("build photo response"),
        Ok(None) => tauri::http::Response::builder()
            .status(404)
            .body(std::borrow::Cow::Owned(Vec::new()))
            .expect("build 404"),
        Err(e) => {
            tracing::warn!("contact-photo lookup for '{id}' failed: {e}");
            tauri::http::Response::builder()
                .status(500)
                .body(std::borrow::Cow::Owned(Vec::new()))
                .expect("build 500")
        }
    }
}

/// Minimal RFC 3986 percent-decoder. Avoids pulling in a dep just
/// to undo what `encodeURIComponent` did on the JS side. Unrecognised
/// `%xx` sequences are passed through verbatim.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// #470 — rebuild the GTK client-side decoration so its buttons
/// accept clicks again.
///
/// On Wayland GTK draws our titlebar itself (GTK3 doesn't implement
/// `xdg-decoration`, so KWin can't decorate us), which makes the
/// minimise / maximise / close buttons part of the app's own surface.
/// A window created with `visible: false` and shown later never gets
/// that decoration wired up properly: the titlebar swallows every
/// click while the page inside stays perfectly interactive. Reported
/// on Kubuntu/KWin, and the same shape as upstream
/// tauri-apps/tauri#11856.
///
/// Toggling resizability re-runs GTK's `update_window_buttons()`,
/// which rebuilds the button box and restores input. Measured against
/// a no-op control on the reporter's machine: without this the buttons
/// are dead on every launch, with it they work immediately.
///
/// Two details are load-bearing:
///   * **Main thread.** GTK is not thread-safe; called from the async
///     runtime the toolkit calls silently do nothing.
///   * **One step per main-loop iteration.** Setting the flag off and
///     on back-to-back is coalesced into a no-op, so the steps are
///     spread across glib idle callbacks — the same shape tao uses for
///     its own maximize (`util::WindowMaximizeProcess`).
///
/// Skipped for maximised and fullscreen windows: those states arrive
/// via a compositor configure, which rebuilds the decoration anyway,
/// and dropping resizability underneath them risks disturbing their
/// geometry.
#[cfg(target_os = "linux")]
fn rebuild_decoration_input_region(win: &tauri::WebviewWindow) {
    if win.is_maximized().unwrap_or(false) || win.is_fullscreen().unwrap_or(false) {
        return;
    }
    let win = win.clone();
    tauri::async_runtime::spawn(async move {
        // Let the map settle first; repairing a window that is still
        // being mapped just gets folded into the map itself.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let marshalled = win.clone().run_on_main_thread(move || {
            use gtk::prelude::*;
            let Ok(gtk_win) = win.gtk_window() else {
                tracing::warn!("#470: gtk_window() unavailable, skipping decoration repair");
                return;
            };
            // Restore whatever the window was actually set to rather
            // than assuming resizable — a fixed-size window must not
            // silently become resizable.
            let was_resizable = gtk_win.is_resizable();
            let step = std::cell::Cell::new(0u8);
            gtk::glib::idle_add_local_full(gtk::glib::Priority::DEFAULT_IDLE, move || {
                match step.get() {
                    0 => {
                        gtk_win.set_resizable(!was_resizable);
                        step.set(1);
                        gtk::glib::ControlFlow::Continue
                    }
                    _ => {
                        gtk_win.set_resizable(was_resizable);
                        gtk::glib::ControlFlow::Break
                    }
                }
            });
        });
        if let Err(e) = marshalled {
            tracing::warn!("#470: could not reach the main thread for decoration repair: {e}");
        }
    });
}

/// No-op off Linux: Windows and macOS draw our titlebar in the
/// compositor, so its buttons can't be affected by anything we do.
#[cfg(not(target_os = "linux"))]
fn rebuild_decoration_input_region(_win: &tauri::WebviewWindow) {}

// ── Native file dialogs (#477) ──────────────────────────────────
//
// The dialog and the file IO it gates both live on the Rust side,
// so no raw filesystem path ever crosses the IPC boundary.  The
// webview can only trigger purpose-specific flows (save this
// attachment, export/import the settings bundle) — a compromised
// webview gets "ask the user for a location", never arbitrary
// file read/write.  The callback-based plugin API is bridged to
// async via a oneshot channel; the `blocking_*` variants would
// pin a runtime worker thread for as long as the dialog is open.

/// Open the native "Save As" dialog. Resolves to `None` when the
/// user cancels.
async fn pick_save_path(
    app: &AppHandle,
    title: &str,
    default_file_name: &str,
    filter: Option<(&str, &[&str])>,
) -> Result<Option<std::path::PathBuf>, UnkaiError> {
    let mut builder = app
        .dialog()
        .file()
        .set_title(title)
        .set_file_name(default_file_name);
    if let Some((name, extensions)) = filter {
        builder = builder.add_filter(name, extensions);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    builder.save_file(move |picked| {
        let _ = tx.send(picked);
    });
    resolve_picked_path(rx).await
}

/// Open the native single-file picker. Resolves to `None` when the
/// user cancels.
async fn pick_open_path(
    app: &AppHandle,
    title: &str,
    filter: (&str, &[&str]),
) -> Result<Option<std::path::PathBuf>, UnkaiError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(title)
        .add_filter(filter.0, filter.1)
        .pick_file(move |picked| {
            let _ = tx.send(picked);
        });
    resolve_picked_path(rx).await
}

/// Await a dialog callback and turn its `FilePath` into a plain
/// `PathBuf`.  The `Url` variant only occurs on mobile platforms,
/// so `into_path` can't fail on our desktop targets — but map the
/// error anyway rather than unwrap.
async fn resolve_picked_path(
    rx: tokio::sync::oneshot::Receiver<Option<tauri_plugin_dialog::FilePath>>,
) -> Result<Option<std::path::PathBuf>, UnkaiError> {
    let picked = rx
        .await
        .map_err(|e| UnkaiError::Other(format!("file dialog closed unexpectedly: {e}")))?;
    match picked {
        Some(file_path) => file_path
            .into_path()
            .map(Some)
            .map_err(|e| UnkaiError::Other(format!("unsupported file location: {e}"))),
        None => Ok(None),
    }
}

/// Custom URI scheme handler for `unkai-logo://localhost/<style>`.
/// Used by the Settings picker to render preview tiles via plain
/// `<img src="unkai-logo://localhost/storm">` — same trick the
/// `contact-photo` scheme uses for avatars.  Unknown style →
/// storm fallback (matches the runtime behaviour, so the preview
/// can't deceive the user).
fn logo_protocol(
    _ctx: UriSchemeContext<'_, tauri::Wry>,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<std::borrow::Cow<'static, [u8]>> {
    let style = request.uri().path().trim_start_matches('/').to_string();
    let bytes = logo_bytes_for(&style);
    tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", "image/png")
        .header("Cache-Control", "private, max-age=300")
        .body(std::borrow::Cow::Borrowed(bytes))
        .expect("build logo response")
}

// ── Profile windows (#535) ──────────────────────────────────────

/// Focus the profile's primary window, creating one (plus its
/// runtime context) when none exists — the rail switcher's "Open
/// in new window".  The context build (SQLCipher key derivation,
/// boot repairs) is real blocking IO, so it runs on the blocking
/// pool before the window work happens on the runtime.
#[tauri::command]
async fn open_profile_window(id: String, app: AppHandle) -> Result<(), UnkaiError> {
    let build_app = app.clone();
    let build_id = id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        windows::ensure_profile_context(&build_app, &build_id)
    })
    .await
    .map_err(|e| UnkaiError::Other(format!("profile open task failed: {e}")))??;
    windows::focus_or_create_profile_window(&app, &id)
}

/// Switch the calling window to another profile in place (#535).
/// Window labels are immutable in Tauri, so the switch is a
/// registry remap: every subsequent command from this window
/// resolves to the new profile.  The frontend follows up by
/// resetting its view state and reloading through the stores.
#[tauri::command]
async fn switch_window_profile(
    id: String,
    app: AppHandle,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    if !windows::is_primary_label(window.label()) {
        return Err(UnkaiError::Other(
            "only a profile's main window can switch profiles".into(),
        ));
    }
    let outgoing = reg.profile_for_label(window.label());
    let build_app = app.clone();
    let build_id = id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        windows::ensure_profile_context(&build_app, &build_id)
    })
    .await
    .map_err(|e| UnkaiError::Other(format!("profile switch task failed: {e}")))??;
    reg.map_window(window.label(), &id);
    // Close the outgoing profile's popouts unless another primary
    // window still shows it: their `parent=` label is THIS window,
    // so a later handoff (Reply from a popped-out mail) would
    // deliver the old profile's data — decrypted body, passphrase
    // — into the new profile's shell.
    if outgoing != id {
        let labels = reg.labels_for_profile(&outgoing);
        let still_shown = labels.iter().any(|l| windows::is_primary_label(l));
        if !still_shown {
            for label in labels {
                if let Some(popout) = app.get_webview_window(&label) {
                    let _ = popout.close();
                }
            }
        }
    }
    // The switched-to profile is now the one the user is looking
    // at — bookmark it and persist the last-used order.
    if reg.note_focused(window.label(), &id) {
        windows::persist_last_used(reg.paths().clone(), id);
    }
    Ok(())
}

/// Map a popout window's label to the calling window's profile
/// BEFORE the popout is created, so its very first command already
/// resolves correctly (#535).  The shared frontend popout helper
/// awaits this ahead of `new WebviewWindow(...)`.
#[tauri::command]
fn register_popout_window(
    label: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    if windows::is_primary_label(&label) {
        return Err(UnkaiError::Other(
            "popout labels must not collide with profile windows".into(),
        ));
    }
    let profile_id = reg.profile_for_label(window.label());
    reg.map_window(&label, &profile_id);
    Ok(())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Restart Unkai in place (#190 follow-up).
///
/// Used by the language-change confirmation popup: paraglide
/// resolves the active locale once, at boot, by walking its
/// strategy chain (`localStorage` → `preferredLanguage` →
/// `baseLocale`), so a runtime language switch can't take full
/// effect without a fresh process.  Tauri's `restart()` calls
/// the platform's "exec same binary" primitive, which is a
/// lot smoother than asking the user to close and reopen
/// manually.
#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

// ── File-association handlers (#254) ────────────────────────────
//
// `bundle.fileAssociations` in `tauri.conf.json` registers Unkai
// with the OS as an "Open with…" candidate for `.ics` and `.eml`.
// When the user double-clicks (or `start file.eml`s) one of those
// the OS launches us with the path as `argv[1]`.  We capture the
// argument once at startup and stash it in `PENDING_FILE_OPEN`;
// the frontend polls `take_pending_file_to_open` after mount and
// routes the path to the right view.

/// One-shot slot for the file the OS handed us at launch time.
/// Frontend takes ownership on its first read so a refresh of the
/// main window doesn't loop into the same import flow.
static PENDING_FILE_OPEN: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();

fn pending_file_slot() -> &'static Mutex<Option<String>> {
    PENDING_FILE_OPEN.get_or_init(|| Mutex::new(None))
}

/// Cold-start buffer for `mailto:` URLs that arrived before the
/// frontend was ready to receive events (#294).  Populated by:
///   - `capture_launch_mailto_arg()` at process start, for cold
///     launches where the OS handed us a mailto as argv[1];
///   - the deep-link plugin's `on_open_url` callback for the
///     very first URL that fires before the webview mounts;
///   - the single-instance plugin when a second launch beats the
///     deep-link path on slower OSes.
///
/// Always a `Vec`, never a single slot, because on a cold start
/// it's plausible (though unusual) for multiple paths to deliver
/// the same URL — the frontend dedups by draining the whole list
/// and parsing each one fresh.
static PENDING_MAILTO_URLS: std::sync::OnceLock<Mutex<Vec<String>>> = std::sync::OnceLock::new();

fn pending_mailto_slot() -> &'static Mutex<Vec<String>> {
    PENDING_MAILTO_URLS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Stash a `mailto:` URL in the cold-start buffer.  No-op if the
/// mutex is poisoned — losing one URL to a worker-thread panic is
/// strictly better than panicking the main thread on lock recovery.
fn buffer_mailto_url(url: &str) {
    if let Ok(mut slot) = pending_mailto_slot().lock() {
        slot.push(url.to_string());
    }
}

/// Capture argv[1] at startup if it points at an `.ics` or `.eml`
/// file we know how to open.  Anything else is ignored — Tauri
/// passes any `--flag` style argv too and we don't want to
/// accidentally treat those as paths.
fn capture_launch_file_arg() {
    // argv is read only to discover a file to *open*, never for an
    // access-control decision — and the candidate is re-validated as
    // an existing `.ics`/`.eml` file below before we act on it.
    // nosemgrep: rust.lang.security.args.args
    let Some(arg) = std::env::args().nth(1) else {
        return;
    };
    if arg.starts_with('-') {
        return;
    }
    let lower = arg.to_lowercase();
    if !(lower.ends_with(".ics") || lower.ends_with(".eml")) {
        return;
    }
    if !std::path::Path::new(&arg).is_file() {
        return;
    }
    if let Ok(mut slot) = pending_file_slot().lock() {
        *slot = Some(arg);
    }
}

/// Capture argv at startup if any argument is a `mailto:` URL.
/// On Windows the OS hands the protocol URL as `argv[1]` when we
/// are the registered handler; on macOS the URL is delivered via
/// the deep-link plugin (which sets up an Apple Event handler);
/// on Linux behaviour depends on the desktop file's `Exec=` line
/// (typically `%u` or `%U` substitution → argv).  Scanning all of
/// argv (not just argv[1]) handles the edge case where a wrapper
/// or shell prepends flags.
fn capture_launch_mailto_arg() {
    // Same rationale as `capture_launch_file_arg`: argv is scanned
    // only to find a `mailto:` URL to act on, not for any security
    // decision.  The value is parsed as a URL, never trusted as auth.
    // nosemgrep: rust.lang.security.args.args
    for arg in std::env::args().skip(1) {
        if arg.to_lowercase().starts_with("mailto:") {
            buffer_mailto_url(&arg);
        }
    }
}

/// Frontend hook: returns the launch-time file path (if any) and
/// clears the slot so a window refresh doesn't re-open it.
#[tauri::command]
fn take_pending_file_to_open() -> Option<String> {
    pending_file_slot()
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

/// Frontend hook: drains the cold-start `mailto:` URL buffer
/// (#294).  Returns the URLs collected so far and clears the
/// buffer — a refresh of the main window won't re-open them.
/// Live URLs arriving after this point are delivered via the
/// `unkai://mailto` Tauri event instead.
#[tauri::command]
fn take_pending_mailto_urls() -> Vec<String> {
    pending_mailto_slot()
        .lock()
        .map(|mut slot| std::mem::take(&mut *slot))
        .unwrap_or_default()
}

/// Cross-platform "open the OS Default Apps panel" — used by the
/// settings page button so users can mark Unkai as the default
/// handler for `.ics` / `.eml` (which OS APIs don't let us do
/// programmatically without a COM dance on Windows).
///
/// - Windows: `start ms-settings:defaultapps` opens the modern
///   Settings panel directly on the Default-Apps page.
/// - macOS: no settings deep-link for default apps; we open the
///   user's home directory in Finder so they can right-click an
///   `.ics` / `.eml` → Get Info → "Open with" → "Change All".
/// - Linux: `xdg-mime default` is the canonical CLI; opening a
///   GUI panel varies wildly across desktops, so we fall back to
///   doing nothing and let the user run the CLI themselves.
#[tauri::command]
fn open_default_apps_settings() -> Result<(), UnkaiError> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", "ms-settings:defaultapps"])
            .spawn()
            .map_err(|e| UnkaiError::Other(format!("open defaults panel: {e}")))?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
            .spawn()
            .map_err(|e| UnkaiError::Other(format!("open defaults panel: {e}")))?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Err(UnkaiError::Other(
            "Default-app registration on Linux is per-desktop; \
             use `xdg-mime default unkai-mail.desktop text/calendar message/rfc822` \
             from a terminal to mark Unkai as the default handler."
                .into(),
        ))
    }
}

#[tauri::command]
fn add_account(
    account: Account,
    password: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::accounts::add_account(account, password, &h.ctx.cache, &h.sync_notify)
}

#[tauri::command]
async fn add_contact_to_category(
    contact_id: String,
    category: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::add_contact_to_category(contact_id, category, &h.ctx.cache).await
}

#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
#[tauri::command]
async fn add_dav_account(
    display_name: String,
    server_url: String,
    username: String,
    password: String,
    use_contacts: bool,
    use_calendars: bool,
    trusted_certs: Option<Vec<unkai_core::models::TrustedCert>>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<NextcloudAccount, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::add_dav_account(
        display_name,
        server_url,
        username,
        password,
        use_contacts,
        use_calendars,
        trusted_certs,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
fn add_local_dav_account(
    display_name: String,
    use_contacts: bool,
    use_calendars: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<NextcloudAccount, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::add_local_dav_account(display_name, use_contacts, use_calendars, &h.ctx.cache)
}

#[tauri::command]
async fn add_talk_participant(
    nc_id: String,
    room_token: String,
    participant: unkai_nextcloud::ParticipantSource,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::talk::add_talk_participant(nc_id, room_token, participant, &h.ctx.cache).await
}

#[tauri::command]
async fn add_talk_participants(
    nc_id: String,
    room_token: String,
    participants: Vec<unkai_nextcloud::ParticipantSource>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::talk::add_talk_participants(nc_id, room_token, participants, &h.ctx.cache).await
}

#[tauri::command]
async fn archive_message(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::archive_message(account_id, folder, uid, &h.ctx.cache).await
}

#[tauri::command]
async fn archive_messages(
    account_id: String,
    folder: String,
    uids: Vec<u32>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<u32>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::archive_messages(account_id, folder, uids, &h.ctx.cache).await
}

#[tauri::command]
async fn check_mail_now(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::check_mail_now(&h.ctx).await
}

#[tauri::command]
fn check_urls(
    urls: Vec<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<LinkVerdict>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::check_urls(urls, &h.ctx.cache, &h.ctx.settings)
}

#[tauri::command]
async fn clear_folder(
    account_id: String,
    folder: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<u32, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::clear_folder(account_id, folder, &h.ctx.cache, h.ctx.ui.as_ref()).await
}

#[tauri::command]
async fn count_outbox(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<u32, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::count_outbox(&h.ctx.cache).await
}

#[tauri::command]
async fn count_outbox_by_account(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<std::collections::HashMap<String, u32>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::count_outbox_by_account(&h.ctx.cache).await
}

#[tauri::command]
async fn create_calendar_event(
    calendar_id: String,
    input: CalendarEventInput,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<CalendarEvent, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::create_calendar_event(calendar_id, input, &h.ctx.cache, h.ctx.ui.as_ref()).await
}

#[tauri::command]
async fn create_contact(
    nc_id: String,
    addressbook_url: String,
    addressbook_name: String,
    input: ContactInput,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Contact, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::create_contact(
        nc_id,
        addressbook_url,
        addressbook_name,
        input,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
async fn create_contact_group(
    nc_id: String,
    addressbook_url: String,
    addressbook_name: String,
    display_name: String,
    member_uids: Vec<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<ContactGroupView, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::create_contact_group(
        nc_id,
        addressbook_url,
        addressbook_name,
        display_name,
        member_uids,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
async fn create_folder(
    account_id: String,
    name: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::create_folder(account_id, name, &h.ctx.cache).await
}

#[tauri::command]
async fn create_nextcloud_calendar(
    nc_id: String,
    display_name: String,
    color: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<CalendarSummary, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::create_nextcloud_calendar(nc_id, display_name, color, &h.ctx.cache).await
}

#[tauri::command]
async fn create_nextcloud_directory(
    nc_id: String,
    path: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::create_nextcloud_directory(nc_id, path, &h.ctx.cache).await
}

#[tauri::command]
async fn create_nextcloud_note(
    nc_id: String,
    title: String,
    content: String,
    category: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<unkai_core::models::Note, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::notes::create_nextcloud_note(nc_id, title, content, category, &h.ctx.cache).await
}

#[allow(clippy::too_many_arguments)] // Tauri command: invoke parameters plus the profile-routing pair
#[tauri::command]
async fn create_nextcloud_share(
    nc_id: String,
    path: String,
    password: Option<String>,
    label: Option<String>,
    permissions: Option<u8>,
    expire_date: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<NextcloudShareResult, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::create_nextcloud_share(
        nc_id,
        path,
        password,
        label,
        permissions,
        expire_date,
        &h.ctx.cache,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
#[tauri::command]
async fn create_nextcloud_task(
    nc_id: String,
    list_id: String,
    summary: String,
    description: Option<String>,
    due_unix: Option<i64>,
    due_tz: Option<String>,
    priority: Option<u8>,
    url: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Task, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::create_nextcloud_task(
        nc_id,
        list_id,
        summary,
        description,
        due_unix,
        due_tz,
        priority,
        url,
        &h.ctx.cache,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
#[tauri::command]
async fn create_nextcloud_task_from_mail(
    nc_id: String,
    list_id: String,
    mail_account_id: String,
    folder: String,
    uid: u32,
    subject: String,
    from: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Task, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::create_nextcloud_task_from_mail(
        nc_id,
        list_id,
        mail_account_id,
        folder,
        uid,
        subject,
        from,
        &h.ctx.cache,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // Tauri command: invoke parameters plus the profile-routing pair
#[tauri::command]
fn create_profile(
    name: String,
    icon: ProfileIcon,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<ProfileMeta, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::profiles::create_profile(h.ctx.ui.as_ref(), name, icon, reg.paths())
}

#[tauri::command]
async fn create_talk_room(
    nc_id: String,
    room_name: String,
    participants: Vec<unkai_nextcloud::ParticipantSource>,
    object_type: Option<String>,
    object_id: Option<String>,
    room_type: Option<u8>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<unkai_nextcloud::TalkRoom, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::talk::create_talk_room(
        nc_id,
        room_name,
        participants,
        object_type,
        object_id,
        room_type,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
fn database_status(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<DatabaseStatusView, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::database_status(&h.ctx.cache, &h.ctx.profile.id)
}

#[tauri::command]
fn debug_link_check(
    url: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<serde_json::Value, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::debug_link_check(url, &h.ctx.cache)
}

#[tauri::command]
async fn decrypt_message(
    account_id: String,
    folder: String,
    uid: u32,
    pgp_passphrase: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Email, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::decrypt_message(account_id, folder, uid, pgp_passphrase, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_calendar_event(
    event_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::delete_calendar_event(event_id, &h.ctx.cache, h.ctx.ui.as_ref()).await
}

#[tauri::command]
async fn delete_contact(
    contact_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::delete_contact(contact_id, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_contact_category(
    name: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::delete_contact_category(name, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_contact_group(
    group_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::delete_contact_group(group_id, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_folder(
    account_id: String,
    name: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::delete_folder(account_id, name, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_message(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::delete_message(account_id, folder, uid, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_nextcloud_calendar(
    calendar_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::delete_nextcloud_calendar(calendar_id, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_nextcloud_note(
    nc_id: String,
    note_id: u64,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::notes::delete_nextcloud_note(nc_id, note_id, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_nextcloud_share(
    nc_id: String,
    share_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::delete_nextcloud_share(nc_id, share_id, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_nextcloud_task(
    nc_id: String,
    list_id: String,
    uid: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::delete_nextcloud_task(nc_id, list_id, uid, &h.ctx.cache).await
}

#[tauri::command]
async fn delete_outbox_entry(
    id: i64,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::delete_outbox_entry(id, &h.ctx.cache, h.ctx.ui.as_ref()).await
}

#[tauri::command]
async fn delete_profile(
    id: String,
    window: tauri::Window,
    app: AppHandle,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    // The body owns the refusal policy; the shell only knows which
    // profiles are live: the caller's own, and every profile some
    // window is currently mapped to (#535).  Pre-flight the policy
    // so a refusal never costs the context teardown below.
    cmds::profiles::ensure_deletable(
        &id,
        &h.ctx.profile.id,
        &reg.profiles_with_windows(),
        reg.paths(),
    )?;
    // A window-less profile may still hold an open runtime context
    // (background sync keeps running after its last window closes)
    // — shut it down so the cache pool releases its files before
    // the secure wipe.
    registry::shutdown_profile_context(&reg, &id).await;
    // Snapshot the refusal set AFTER the await: a window opened
    // for the target profile during the shutdown must trip the
    // body's re-check, not slip past a stale vector.
    let open = reg.profiles_with_windows();
    let active = h.ctx.profile.id.clone();
    let paths = reg.paths().clone();
    let ui = h.ctx.ui.clone();
    let delete_id = id.clone();
    // The secure wipe random-overwrites the whole cache.db — real
    // blocking IO that must not pin an async runtime worker.
    let result = tauri::async_runtime::spawn_blocking(move || {
        cmds::profiles::delete_profile(ui.as_ref(), delete_id, &active, &open, &paths)
    })
    .await
    .map_err(|e| UnkaiError::Other(format!("profile delete task failed: {e}")))?;
    if result.is_err() {
        // The profile is still listed but its context is gone —
        // best-effort rebuild so its background sync resumes
        // instead of staying silently dead until a window opens.
        if let Err(e) = windows::ensure_profile_context(&app, &id) {
            tracing::warn!("could not reopen profile '{id}' after a failed delete: {e}");
        }
    }
    // Repaint the tray badge without the deleted profile's
    // contribution (`remove_profile` dropped its unread entry).
    refresh_unread_badge(&h.ctx.cache, h.ctx.ui.as_ref());
    result
}

#[tauri::command]
async fn delete_talk_room(
    nc_id: String,
    room_token: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::talk::delete_talk_room(nc_id, room_token, &h.ctx.cache).await
}

#[tauri::command]
async fn detect_jmap(host: String) -> Result<Option<String>, UnkaiError> {
    cmds::accounts::detect_jmap(host).await
}

#[tauri::command]
async fn detect_nc_maps(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<NextcloudMapsCapability, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::detect_nc_maps(nc_id, &h.ctx.cache).await
}

#[tauri::command]
fn disable_fido_only_mode(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::disable_fido_only_mode(&h.ctx.cache, &h.ctx.profile.id)
}

#[tauri::command]
async fn discover_account_settings(
    email: String,
) -> Result<Option<unkai_discovery::DiscoveredAccount>, UnkaiError> {
    cmds::accounts::discover_account_settings(email).await
}

#[tauri::command]
async fn dismiss_cancelled_event(
    uid: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::dismiss_cancelled_event(uid, &h.ctx.cache).await
}

#[tauri::command]
fn dismiss_event_reminder(
    uid: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::dismiss_event_reminder(uid, &h.ctx.reminders)
}

#[tauri::command]
async fn download_calendar_from_message(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<Vec<u8>>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::download_calendar_from_message(account_id, folder, uid, &h.ctx.cache).await
}

#[tauri::command]
async fn download_decrypted_attachment(
    account_id: String,
    folder: String,
    uid: u32,
    part_id: u32,
    pgp_passphrase: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<u8>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::download_decrypted_attachment(
        account_id,
        folder,
        uid,
        part_id,
        pgp_passphrase,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
async fn download_email_attachment(
    account_id: String,
    folder: String,
    uid: u32,
    part_id: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<u8>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::download_email_attachment(account_id, folder, uid, part_id, &h.ctx.cache).await
}

#[tauri::command]
async fn download_nextcloud_file(
    nc_id: String,
    path: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<u8>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::download_nextcloud_file(nc_id, path, &h.ctx.cache).await
}

#[tauri::command]
async fn edit_outbox_entry(
    id: i64,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<OutboxRowDto, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::edit_outbox_entry(id, &h.ctx.cache, h.ctx.ui.as_ref()).await
}

#[tauri::command]
fn enable_fido_only_mode(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::enable_fido_only_mode(&h.ctx.profile.id)
}

#[tauri::command]
async fn expunge_draft_after_send(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::expunge_draft_after_send(account_id, folder, uid, &h.ctx.cache).await
}

/// #477 — "Download settings" with the save dialog on the Rust
/// side. Returns the chosen path (for the "Saved to …" toast) or
/// `None` when the user cancels.
#[tauri::command]
async fn export_settings_bundle(
    local_storage: std::collections::HashMap<String, String>,
    app: AppHandle,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<String>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    let Some(path) = pick_save_path(
        &app,
        "Save Unkai settings backup",
        "unkai-settings.json",
        Some(("Unkai settings", &["json"])),
    )
    .await?
    else {
        return Ok(None);
    };
    cmds::settings::export_settings_bundle_to_path(
        &path,
        local_storage,
        &h.ctx.cache,
        &h.ctx.profile,
    )
    .await?;
    Ok(Some(path.display().to_string()))
}

#[tauri::command]
async fn fetch_envelopes(
    account_id: String,
    folder: String,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::fetch_envelopes(account_id, folder, limit, &h.ctx.cache).await
}

#[tauri::command]
async fn fetch_folders(
    account_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<Folder>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::fetch_folders(account_id, &h.ctx.cache).await
}

#[tauri::command]
async fn fetch_inline_images(
    account_id: String,
    folder: String,
    uid: u32,
    pgp_passphrase: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<InlineImageView>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::fetch_inline_images(account_id, folder, uid, pgp_passphrase, &h.ctx.cache).await
}

#[tauri::command]
async fn fetch_message(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Email, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::fetch_message(h.ctx.ui.as_ref(), account_id, folder, uid, &h.ctx.cache).await
}

#[tauri::command]
async fn fetch_older_envelopes(
    account_id: String,
    folder: String,
    before_uid: u32,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::fetch_older_envelopes(account_id, folder, before_uid, limit, &h.ctx.cache).await
}

#[tauri::command]
async fn fetch_older_unified_envelopes(
    folder: String,
    before_uid_per_account: std::collections::HashMap<String, u32>,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::fetch_older_unified_envelopes(folder, before_uid_per_account, limit, &h.ctx.cache)
        .await
}

#[tauri::command]
async fn fetch_unified_envelopes(
    folder: String,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::fetch_unified_envelopes(folder, limit, &h.ctx.cache).await
}

#[tauri::command]
async fn fetch_unified_special_envelopes(
    special: String,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::fetch_unified_special_envelopes(special, limit, &h.ctx.cache).await
}

#[tauri::command]
fn fido_enroll(
    credential_id_b64: String,
    salt_b64: String,
    prf_output_b64: String,
    label: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::fido_enroll(
        credential_id_b64,
        salt_b64,
        prf_output_b64,
        label,
        &h.ctx.cache,
        &h.ctx.profile.id,
    )
}

#[tauri::command]
fn fido_enroll_passphrase(
    passphrase: String,
    label: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::fido_enroll_passphrase(passphrase, label, &h.ctx.cache, &h.ctx.profile.id)
}

#[tauri::command]
fn fido_generate_salt() -> Result<String, UnkaiError> {
    cmds::settings::fido_generate_salt()
}

#[tauri::command]
fn fido_remove(
    credential_id_b64: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::fido_remove(credential_id_b64, &h.ctx.profile.id)
}

#[tauri::command]
fn fido_status(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<FidoStatusView, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::fido_status(&h.ctx.profile.id)
}

#[tauri::command]
fn fido_verify_passphrase(
    passphrase: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<bool, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::fido_verify_passphrase(passphrase, &h.ctx.profile.id)
}

#[tauri::command]
fn fido_verify_prf(
    credential_id_b64: String,
    prf_output_b64: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<bool, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::fido_verify_prf(credential_id_b64, prf_output_b64, &h.ctx.profile.id)
}

#[tauri::command]
async fn find_nextcloud_user_by_email(
    nc_id: String,
    email: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<NextcloudUserLookup>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::find_nextcloud_user_by_email(nc_id, email, &h.ctx.cache).await
}

#[tauri::command]
async fn geocode_search(
    query: String,
    lang: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<cmds::geocode::GeocodeResult>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::geocode_search(query, lang, &h.ctx.cache, &h.ctx.settings).await
}

#[tauri::command]
fn get_accounts(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<Account>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::accounts::get_accounts(&h.ctx.cache)
}

#[tauri::command]
async fn get_app_settings(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<AppSettings, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::get_app_settings(&h.ctx.settings).await
}

#[tauri::command]
fn get_attachment_previews(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<AttachmentPreviewView>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_attachment_previews(account_id, folder, uid, &h.ctx.cache)
}

#[tauri::command]
async fn get_attendee_availability(
    nc_id: String,
    attendee_emails: Vec<String>,
    range_start: chrono::DateTime<chrono::Utc>,
    range_end: chrono::DateTime<chrono::Utc>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<AttendeeAvailability>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::get_attendee_availability(
        nc_id,
        attendee_emails,
        range_start,
        range_end,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
fn get_cached_calendars(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<CalendarSummary>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::get_cached_calendars(nc_id, &h.ctx.cache)
}

#[tauri::command]
fn get_cached_envelopes(
    account_id: String,
    folder: String,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_cached_envelopes(account_id, folder, limit, &h.ctx.cache)
}

#[tauri::command]
fn get_cached_events(
    calendar_ids: Vec<String>,
    range_start: chrono::DateTime<chrono::Utc>,
    range_end: chrono::DateTime<chrono::Utc>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<CalendarEvent>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::get_cached_events(calendar_ids, range_start, range_end, &h.ctx.cache)
}

#[tauri::command]
fn get_cached_folders(
    account_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<Folder>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_cached_folders(account_id, &h.ctx.cache)
}

#[tauri::command]
fn get_cached_message(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<Email>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_cached_message(account_id, folder, uid, &h.ctx.cache)
}

#[tauri::command]
fn get_calendars_sync_status(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<SyncStatus, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::get_calendars_sync_status(nc_id, &h.ctx.cache)
}

#[tauri::command]
fn get_contact_photo(
    contact_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<ContactPhoto>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::get_contact_photo(contact_id, &h.ctx.cache)
}

#[tauri::command]
fn get_contacts(
    nc_id: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<Contact>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::get_contacts(nc_id, &h.ctx.cache)
}

#[tauri::command]
fn get_contacts_sync_status(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<SyncStatus, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::get_contacts_sync_status(nc_id, &h.ctx.cache)
}

#[tauri::command]
fn get_current_profile(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<String, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    Ok(cmds::profiles::get_current_profile(&h.ctx.profile))
}

#[tauri::command]
fn get_envelopes_by_thread(
    account_id: String,
    folder: String,
    thread_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_envelopes_by_thread(account_id, folder, thread_id, &h.ctx.cache)
}

#[tauri::command]
async fn get_event_partstat_for_user(
    uid: String,
    attendee_hint: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<String>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::get_event_partstat_for_user(uid, attendee_hint, &h.ctx.cache).await
}

#[tauri::command]
fn get_link_check_status(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<link_check::UrlhausStatus, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_link_check_status(&h.ctx.cache)
}

#[tauri::command]
fn get_nextcloud_accounts(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<NextcloudAccount>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::get_nextcloud_accounts(&h.ctx.cache)
}

#[tauri::command]
async fn get_nextcloud_note(
    nc_id: String,
    note_id: u64,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<unkai_core::models::Note, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::notes::get_nextcloud_note(nc_id, note_id, &h.ctx.cache).await
}

#[tauri::command]
async fn get_nextcloud_user_email(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<String>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::get_nextcloud_user_email(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn get_receipt_status(
    account_id: String,
    message_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<unkai_store::SentReceiptStatus>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_receipt_status(account_id, message_id, &h.ctx.cache).await
}

#[tauri::command]
async fn get_rsvp_response(
    uid: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<String>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::get_rsvp_response(uid, &h.ctx.cache).await
}

#[tauri::command]
fn get_settings_sync_state(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<SettingsSyncStateView, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::get_settings_sync_state(&h.ctx.profile)
}

#[tauri::command]
fn get_startup_mode(reg: State<'_, ProfileRegistry>) -> Result<StartupMode, UnkaiError> {
    cmds::profiles::get_startup_mode(reg.paths())
}

#[tauri::command]
fn get_tasks_sync_status(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<SyncStatus, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::get_tasks_sync_status(nc_id, &h.ctx.cache)
}

#[tauri::command]
fn get_total_unread(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<u32, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_total_unread(&h.ctx.cache)
}

#[tauri::command]
fn get_unified_cached_envelopes(
    folder: String,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_unified_cached_envelopes(folder, limit, &h.ctx.cache)
}

#[tauri::command]
fn get_unified_special_cached_envelopes(
    special: String,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_unified_special_cached_envelopes(special, limit, &h.ctx.cache)
}

#[tauri::command]
fn get_unread_counts_by_account(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<std::collections::HashMap<String, u32>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::get_unread_counts_by_account(&h.ctx.cache)
}

#[tauri::command]
fn get_wipe_policy(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<WipePolicyView, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::get_wipe_policy(&h.ctx.profile.id)
}

#[tauri::command]
async fn import_calendar_file(
    calendar_id: String,
    path: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<ImportCalendarReport, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::import_calendar_file(calendar_id, path, &h.ctx.cache, h.ctx.ui.as_ref()).await
}

#[tauri::command]
async fn import_contacts_file(
    nc_id: String,
    addressbook_url: String,
    addressbook_name: String,
    path: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<ImportContactsReport, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::import_contacts_file(
        nc_id,
        addressbook_url,
        addressbook_name,
        path,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
async fn import_custom_theme(
    source_path: String,
    label: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<CustomTheme, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::import_custom_theme(
        h.ctx.ui.as_ref(),
        source_path,
        label,
        &h.ctx.settings,
        &h.ctx.profile,
    )
    .await
}

/// Result of a completed `import_settings_bundle`: where the bundle
/// came from (for the "Imported from …" confirmation) plus its
/// `localStorage` portion for the frontend to mirror into its own
/// storage.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsBundleImport {
    path: String,
    local_storage: std::collections::HashMap<String, String>,
}

/// #477 — "Import settings" with the file picker on the Rust side.
/// Returns `None` when the user cancels the picker.
#[tauri::command]
async fn import_settings_bundle(
    app: AppHandle,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<SettingsBundleImport>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    let Some(path) = pick_open_path(
        &app,
        "Import Unkai h.ctx.settings backup",
        ("Unkai h.ctx.settings", &["json"]),
    )
    .await?
    else {
        return Ok(None);
    };
    let local_storage = cmds::settings::import_settings_bundle_from_path(
        &path,
        &h.ctx.cache,
        &h.ctx.settings,
        &h.mcp,
        &h.ctx.profile,
    )
    .await?;
    Ok(Some(SettingsBundleImport {
        path: path.display().to_string(),
        local_storage,
    }))
}

#[tauri::command]
fn is_event_in_calendar(
    uid: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<bool, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::is_event_in_calendar(uid, &h.ctx.cache)
}

#[tauri::command]
fn is_invite_cancelled(
    uid: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<bool, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::is_invite_cancelled(uid, &h.ctx.cache)
}

#[tauri::command]
async fn list_all_outbox(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<OutboxRowDto>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::list_all_outbox(&h.ctx.cache).await
}

#[tauri::command]
fn list_contact_categories(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<ContactCategoryView>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::list_contact_categories(&h.ctx.cache)
}

#[tauri::command]
fn list_contact_groups(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<ContactGroupView>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::list_contact_groups(&h.ctx.cache)
}

#[tauri::command]
async fn list_mailing_lists(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<MailingListView>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::list_mailing_lists(&h.ctx.cache).await
}

#[tauri::command]
async fn list_nextcloud_addressbooks(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<AddressbookSummary>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::list_nextcloud_addressbooks(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn list_nextcloud_calendars(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<CalendarSummary>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::list_nextcloud_calendars(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn list_nextcloud_files(
    nc_id: String,
    path: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<FileEntry>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::list_nextcloud_files(nc_id, path, &h.ctx.cache).await
}

#[tauri::command]
async fn list_nextcloud_groups(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<NextcloudGroupView>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::list_nextcloud_groups(&h.ctx.cache).await
}

#[tauri::command]
fn list_nextcloud_notes(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<unkai_core::models::Note>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::notes::list_nextcloud_notes(nc_id, &h.ctx.cache)
}

#[tauri::command]
async fn list_nextcloud_shares(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<NextcloudShareRow>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::list_nextcloud_shares(nc_id, &h.ctx.cache).await
}

#[tauri::command]
fn list_nextcloud_task_lists(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<TaskList>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::list_nextcloud_task_lists(nc_id, &h.ctx.cache)
}

#[tauri::command]
fn list_nextcloud_tasks(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<Task>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::list_nextcloud_tasks(nc_id, &h.ctx.cache)
}

#[tauri::command]
async fn list_outbox(
    account_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<OutboxRowDto>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::list_outbox(account_id, &h.ctx.cache).await
}

#[tauri::command]
fn list_profiles(reg: State<'_, ProfileRegistry>) -> Result<Vec<ProfileMeta>, UnkaiError> {
    cmds::profiles::list_profiles(reg.paths())
}

#[tauri::command]
fn list_provider_presets() -> Vec<unkai_discovery::ProviderPreset> {
    cmds::accounts::list_provider_presets()
}

#[tauri::command]
async fn list_system_fonts(cache: State<'_, SystemFontsCache>) -> Result<Vec<String>, UnkaiError> {
    cmds::system::list_system_fonts(&cache).await
}

#[tauri::command]
async fn list_talk_rooms(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<unkai_nextcloud::TalkRoom>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::talk::list_talk_rooms(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn mark_as_read(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::mark_as_read(account_id, folder, uid, &h.ctx.cache, h.ctx.ui.as_ref()).await
}

#[tauri::command]
async fn mark_folder_read(
    account_id: String,
    folder: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::mark_folder_read(account_id, folder, &h.ctx.cache, h.ctx.ui.as_ref()).await
}

#[tauri::command]
async fn mcp_generate_token(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<String, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::mcp_generate_token(&h.mcp, &h.ctx.profile.id).await
}

#[tauri::command]
async fn mcp_list_tools(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<McpToolView>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::mcp_list_tools(&h.ctx.settings).await
}

#[tauri::command]
async fn mcp_revoke_token(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::mcp_revoke_token(&h.mcp, &h.ctx.profile.id).await
}

#[tauri::command]
async fn mcp_server_status(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<McpServerStatus, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::mcp_server_status(&h.mcp).await
}

#[tauri::command]
async fn mcp_token_status(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<bool, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::mcp_token_status(&h.ctx.profile.id).await
}

#[tauri::command]
async fn move_message(
    account_id: String,
    folder: String,
    uid: u32,
    dest_folder: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::move_message(account_id, folder, uid, dest_folder, &h.ctx.cache).await
}

#[tauri::command]
async fn move_messages(
    account_id: String,
    folder: String,
    uids: Vec<u32>,
    dest_folder: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<u32>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::move_messages(account_id, folder, uids, dest_folder, &h.ctx.cache).await
}

#[tauri::command]
async fn nc_probe_settings_bundle(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::nc_probe_settings_bundle(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn nc_restore_settings_bundle(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<std::collections::HashMap<String, String>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::nc_restore_settings_bundle(nc_id, &h.ctx.cache, &h.ctx.settings, &h.ctx.profile)
        .await
}

#[tauri::command]
async fn nextcloud_file_preview(
    nc_id: String,
    path: String,
    size: Option<u32>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<Vec<u8>>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::nextcloud_file_preview(nc_id, path, size, &h.ctx.cache).await
}

#[tauri::command]
async fn notify_settings_changed(
    local_storage: std::collections::HashMap<String, String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::notify_settings_changed(local_storage, &h.local_storage, &h.sync_notify).await
}

#[tauri::command]
async fn office_close_attachment(
    nc_id: String,
    temp_path: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::system::office_close_attachment(nc_id, temp_path, &h.ctx.cache).await
}

#[tauri::command]
async fn office_open_attachment(
    nc_id: String,
    filename: String,
    data: Vec<u8>,
    content_type: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<OfficeOpenResult, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::system::office_open_attachment(nc_id, filename, data, content_type, &h.ctx.cache).await
}

#[tauri::command]
async fn office_sweep_temp(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<u32, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::system::office_sweep_temp(nc_id, &h.ctx.cache).await
}

#[tauri::command]
fn open_url(url: String) -> Result<(), UnkaiError> {
    cmds::system::open_url(url)
}

#[tauri::command]
fn parse_eml_file(path: String) -> Result<unkai_core::models::Email, UnkaiError> {
    cmds::mail::parse_eml_file(path)
}

#[tauri::command]
fn parse_eml_file_inline_images(path: String) -> Result<Vec<InlineImageView>, UnkaiError> {
    cmds::mail::parse_eml_file_inline_images(path)
}

#[tauri::command]
fn parse_event_invite(bytes: Vec<u8>) -> Result<InviteSummary, UnkaiError> {
    cmds::calendar::parse_event_invite(bytes)
}

#[tauri::command]
fn parse_ics_file(path: String) -> Result<Vec<unkai_core::models::CalendarEvent>, UnkaiError> {
    cmds::calendar::parse_ics_file(path)
}

#[tauri::command]
async fn pdf_close_attachment(
    nc_id: String,
    temp_path: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::system::pdf_close_attachment(nc_id, temp_path, &h.ctx.cache).await
}

#[tauri::command]
async fn pdf_open_attachment(
    nc_id: String,
    filename: String,
    data: Vec<u8>,
    content_type: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<PdfOpenResult, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::system::pdf_open_attachment(nc_id, filename, data, content_type, &h.ctx.cache).await
}

#[tauri::command]
fn pgp_disable_unlock_automatically(account_id: String) -> Result<(), UnkaiError> {
    cmds::crypto::pgp_disable_unlock_automatically(account_id)
}

#[tauri::command]
fn pgp_enable_unlock_automatically(
    account_id: String,
    passphrase: String,
) -> Result<(), UnkaiError> {
    cmds::crypto::pgp_enable_unlock_automatically(account_id, passphrase)
}

#[tauri::command]
fn pgp_get_account_key_status(
    account_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<PgpKeyStatus, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::pgp_get_account_key_status(account_id, &h.ctx.cache)
}

#[tauri::command]
fn pgp_get_keys_for_email(
    email: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<PgpPublicKeyDto>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::pgp_get_keys_for_email(email, &h.ctx.cache)
}

#[tauri::command]
fn pgp_has_unlock_automatically(account_id: String) -> Result<bool, UnkaiError> {
    cmds::crypto::pgp_has_unlock_automatically(account_id)
}

#[tauri::command]
async fn pgp_import_private_key(
    account_id: String,
    armored_key: String,
    passphrase: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<String, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::pgp_import_private_key(
        account_id,
        armored_key,
        passphrase,
        &h.ctx.cache,
        &h.sync_notify,
    )
    .await
}

#[tauri::command]
fn pgp_import_public_key(
    armored_key: String,
    email_hint: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<String, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::pgp_import_public_key(armored_key, email_hint, &h.ctx.cache)
}

#[tauri::command]
fn pgp_list_public_keys(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<PgpPublicKeyDto>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::pgp_list_public_keys(&h.ctx.cache)
}

#[tauri::command]
fn pgp_remove_private_key(
    account_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::pgp_remove_private_key(account_id, &h.ctx.cache, &h.sync_notify)
}

#[tauri::command]
fn pgp_remove_public_key(
    fingerprint: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::pgp_remove_public_key(fingerprint, &h.ctx.cache)
}

#[tauri::command]
async fn poll_nextcloud_login(
    poll_endpoint: String,
    poll_token: String,
    trusted_certs: Option<Vec<unkai_core::models::TrustedCert>>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<NextcloudAccount>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::poll_nextcloud_login(poll_endpoint, poll_token, trusted_certs, &h.ctx.cache)
        .await
}

#[tauri::command]
async fn print_attachment(file_name: String, bytes: Vec<u8>) -> Result<(), UnkaiError> {
    cmds::system::print_attachment(file_name, bytes).await
}

#[tauri::command]
async fn probe_server_certificate(host: String, port: u16) -> Result<ProbedCert, UnkaiError> {
    cmds::accounts::probe_server_certificate(host, port).await
}

#[allow(clippy::too_many_arguments)] // Tauri command: invoke parameters plus the profile-routing pair
#[tauri::command]
fn put_attachment_preview(
    account_id: String,
    folder: String,
    uid: u32,
    part_id: u32,
    mime: String,
    base64: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::put_attachment_preview(account_id, folder, uid, part_id, mime, base64, &h.ctx.cache)
}

#[tauri::command]
fn record_cancelled_invite(
    uid: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::record_cancelled_invite(uid, &h.ctx.cache)
}

#[tauri::command]
async fn refresh_nextcloud_capabilities(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<NextcloudAccount, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::refresh_nextcloud_capabilities(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn refresh_urlhaus_now(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<u32, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::refresh_urlhaus_now(&h.ctx.cache).await
}

#[tauri::command]
fn remove_account(
    id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::accounts::remove_account(id, &h.ctx.cache, &h.sync_notify)
}

#[tauri::command]
async fn remove_contact_from_category(
    contact_id: String,
    category: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::remove_contact_from_category(contact_id, category, &h.ctx.cache).await
}

#[tauri::command]
async fn remove_custom_theme(
    id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::remove_custom_theme(h.ctx.ui.as_ref(), id, &h.ctx.settings, &h.ctx.profile)
        .await
}

#[tauri::command]
fn remove_nextcloud_account(
    id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::remove_nextcloud_account(id, &h.ctx.cache)
}

#[tauri::command]
async fn rename_contact_category(
    old: String,
    new: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::rename_contact_category(old, new, &h.ctx.cache).await
}

#[tauri::command]
async fn rename_folder(
    account_id: String,
    old_name: String,
    new_name: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::rename_folder(account_id, old_name, new_name, &h.ctx.cache).await
}

#[tauri::command]
async fn rename_mailing_list(
    id: String,
    new_name: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::rename_mailing_list(id, new_name, &h.ctx.cache).await
}

#[tauri::command]
async fn rename_talk_room(
    nc_id: String,
    room_token: String,
    new_name: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::talk::rename_talk_room(nc_id, room_token, new_name, &h.ctx.cache).await
}

#[tauri::command]
async fn respond_mdn_request(
    account_id: String,
    folder: String,
    uid: u32,
    decline: bool,
    automatic: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::respond_mdn_request(account_id, folder, uid, decline, automatic, &h.ctx.cache).await
}

#[tauri::command]
async fn respond_to_invite(
    calendar_id: String,
    raw_ics: String,
    partstat: String,
    attendee_hint: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::respond_to_invite(calendar_id, raw_ics, partstat, attendee_hint, &h.ctx.cache)
        .await
}

#[tauri::command]
async fn retry_outbox_entry(
    id: i64,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::retry_outbox_entry(id, &h.ctx).await
}

#[tauri::command]
async fn retry_outbox_entry_with_passphrase(
    id: i64,
    pgp_passphrase: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::retry_outbox_entry_with_passphrase(id, pgp_passphrase, &h.ctx).await
}

#[tauri::command]
async fn rsvp_existing_event(
    event_id: String,
    partstat: String,
    attendee_hint: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::rsvp_existing_event(event_id, partstat, attendee_hint, &h.ctx.cache).await
}

/// #477 — attachment Download with the "Save As" dialog on the
/// Rust side. Dialog first, so a cancel costs no network fetch;
/// then the fetch + write happen entirely in the backend. Returns
/// the chosen path, or `None` when the user cancels.
#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
#[tauri::command]
async fn save_attachment_as(
    account_id: String,
    folder: String,
    uid: u32,
    part_id: u32,
    file_name: String,
    pgp_passphrase: Option<String>,
    app: AppHandle,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<String>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    let Some(path) = pick_save_path(&app, "Save attachment", &file_name, None).await? else {
        return Ok(None);
    };
    cmds::system::save_attachment_to_path(
        &path,
        account_id,
        folder,
        uid,
        part_id,
        pgp_passphrase,
        &h.ctx.cache,
    )
    .await?;
    Ok(Some(path.display().to_string()))
}

#[tauri::command]
async fn save_draft(
    account_id: String,
    email: OutgoingEmail,
    replace_source: Option<DraftReplaceSource>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<SavedDraft, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::save_draft(account_id, email, replace_source, &h.ctx.cache).await
}

#[tauri::command]
fn search_contacts(
    query: String,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<Contact>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::search_contacts(query, limit, &h.ctx.cache)
}

#[tauri::command]
fn search_emails(
    query: String,
    scope: Option<SearchScope>,
    filters: Option<SearchFilters>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<SearchHit>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::search_emails(query, scope, filters, &h.ctx.cache)
}

#[tauri::command]
async fn search_imap_server(
    account_id: String,
    folder: String,
    query: String,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::search_imap_server(account_id, folder, query, limit, &h.ctx.cache).await
}

#[tauri::command]
async fn search_imap_server_older(
    account_id: String,
    folder: String,
    query: String,
    before_uid: u32,
    limit: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<EmailEnvelope>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::search_imap_server_older(account_id, folder, query, before_uid, limit, &h.ctx.cache)
        .await
}

#[tauri::command]
async fn send_email(
    account_id: String,
    email: OutgoingEmail,
    replied_to: Option<RepliedToRef>,
    outbox_source: Option<OutboxSourceRef>,
    pgp_passphrase: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<i64, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::send_email(
        account_id,
        email,
        replied_to,
        outbox_source,
        pgp_passphrase,
        &h.ctx,
    )
    .await
}

#[tauri::command]
fn set_account_password(id: String, password: String) -> Result<(), UnkaiError> {
    cmds::accounts::set_account_password(id, password)
}

#[tauri::command]
fn set_category_use_as_mailing_list(
    name: String,
    enabled: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::set_category_use_as_mailing_list(name, enabled, &h.ctx.cache)
}

#[tauri::command]
fn set_contact_group_emoji(
    group_id: String,
    emoji: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::set_contact_group_emoji(group_id, emoji, &h.ctx.cache)
}

#[tauri::command]
fn set_contact_group_hidden(
    group_id: String,
    hidden: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::set_contact_group_hidden(group_id, hidden, &h.ctx.cache)
}

#[tauri::command]
fn set_folder_icon(
    account_id: String,
    folder_name: String,
    icon: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::set_folder_icon(account_id, folder_name, icon, &h.ctx.cache, &h.sync_notify)
}

#[tauri::command]
async fn set_logo_style(
    style: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::set_logo_style(h.ctx.ui.as_ref(), style, &h.ctx.settings, &h.ctx.profile).await
}

#[tauri::command]
fn set_mailing_list_emoji(
    id: String,
    emoji: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::set_mailing_list_emoji(id, emoji, &h.ctx.cache)
}

#[tauri::command]
fn set_mailing_list_hidden(
    id: String,
    hidden: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::set_mailing_list_hidden(id, hidden, &h.ctx.cache)
}

#[tauri::command]
async fn set_message_flagged(
    account_id: String,
    folder: String,
    uid: u32,
    flagged: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::set_message_flagged(account_id, folder, uid, flagged, &h.ctx.cache).await
}

#[tauri::command]
async fn set_message_pinned(
    account_id: String,
    folder: String,
    uid: u32,
    pinned: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::set_message_pinned(account_id, folder, uid, pinned, &h.ctx.cache).await
}

#[tauri::command]
async fn set_message_priority(
    account_id: String,
    folder: String,
    uid: u32,
    priority: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::set_message_priority(account_id, folder, uid, priority, &h.ctx.cache).await
}

#[tauri::command]
async fn set_message_read(
    account_id: String,
    folder: String,
    uid: u32,
    read: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::set_message_read(
        account_id,
        folder,
        uid,
        read,
        &h.ctx.cache,
        h.ctx.ui.as_ref(),
    )
    .await
}

#[tauri::command]
async fn set_message_reminder(
    account_id: String,
    folder: String,
    uid: u32,
    remind_at: Option<i64>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::mail::set_message_reminder(account_id, folder, uid, remind_at, &h.ctx.cache).await
}

#[tauri::command]
fn set_nextcloud_calendar_hidden(
    calendar_id: String,
    hidden: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::set_nextcloud_calendar_hidden(calendar_id, hidden, &h.ctx.cache)
}

#[tauri::command]
fn set_nextcloud_calendar_muted(
    calendar_id: String,
    muted: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::set_nextcloud_calendar_muted(calendar_id, muted, &h.ctx.cache)
}

#[tauri::command]
fn set_nextcloud_task_list_hidden(
    task_list_id: String,
    hidden: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::set_nextcloud_task_list_hidden(task_list_id, hidden, &h.ctx.cache)
}

#[tauri::command]
fn set_nextcloud_task_list_muted(
    task_list_id: String,
    muted: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::set_nextcloud_task_list_muted(task_list_id, muted, &h.ctx.cache)
}

#[tauri::command]
async fn set_settings_sync_target(
    target_nc_id: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::set_settings_sync_target(target_nc_id, &h.sync_notify, &h.ctx.profile).await
}

#[tauri::command]
fn set_startup_mode(
    mode: StartupMode,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::profiles::set_startup_mode(h.ctx.ui.as_ref(), mode, reg.paths())
}

#[tauri::command]
async fn set_talk_room_public(
    nc_id: String,
    room_token: String,
    public: bool,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::talk::set_talk_room_public(nc_id, room_token, public, &h.ctx.cache).await
}

#[tauri::command]
fn set_wipe_policy(
    policy: WipePolicyView,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::set_wipe_policy(policy, &h.ctx.profile.id)
}

#[tauri::command]
fn smime_disable_unlock_automatically(account_id: String) -> Result<(), UnkaiError> {
    cmds::crypto::smime_disable_unlock_automatically(account_id)
}

#[tauri::command]
fn smime_enable_unlock_automatically(
    account_id: String,
    passphrase: String,
) -> Result<(), UnkaiError> {
    cmds::crypto::smime_enable_unlock_automatically(account_id, passphrase)
}

#[tauri::command]
fn smime_get_account_cert_status(
    account_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<SmimeCertStatus, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::smime_get_account_cert_status(account_id, &h.ctx.cache)
}

#[tauri::command]
fn smime_get_certs_for_email(
    email: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<SmimeCertDto>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::smime_get_certs_for_email(email, &h.ctx.cache)
}

#[tauri::command]
fn smime_has_unlock_automatically(account_id: String) -> Result<bool, UnkaiError> {
    cmds::crypto::smime_has_unlock_automatically(account_id)
}

#[tauri::command]
fn smime_import_pkcs12(
    account_id: String,
    pkcs12_base64: String,
    passphrase: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<String, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::smime_import_pkcs12(
        account_id,
        pkcs12_base64,
        passphrase,
        &h.ctx.cache,
        &h.sync_notify,
    )
}

#[tauri::command]
fn smime_import_public_cert(
    cert_data: String,
    email_hint: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<String, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::smime_import_public_cert(cert_data, email_hint, &h.ctx.cache)
}

#[tauri::command]
fn smime_list_public_certs(
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<SmimeCertDto>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::smime_list_public_certs(&h.ctx.cache)
}

#[tauri::command]
fn smime_remove_private_cert(
    account_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::smime_remove_private_cert(account_id, &h.ctx.cache, &h.sync_notify)
}

#[tauri::command]
fn smime_remove_public_cert(
    fingerprint: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::smime_remove_public_cert(fingerprint, &h.ctx.cache)
}

#[tauri::command]
fn snooze_event_reminder(
    uid: String,
    snooze_until_iso: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::snooze_event_reminder(uid, snooze_until_iso, &h.ctx.reminders)
}

#[tauri::command]
async fn start_nextcloud_login(
    server_url: String,
    trusted_certs: Option<Vec<unkai_core::models::TrustedCert>>,
) -> Result<LoginFlowInit, UnkaiError> {
    cmds::nextcloud::start_nextcloud_login(server_url, trusted_certs).await
}

#[tauri::command]
async fn sync_calendar_by_id(
    calendar_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::sync_calendar_by_id(calendar_id, &h.ctx.cache).await
}

#[tauri::command]
async fn sync_nextcloud_calendars(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<SyncCalendarsReport, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::sync_nextcloud_calendars(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn sync_nextcloud_contacts(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<SyncContactsReport, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::sync_nextcloud_contacts(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn sync_nextcloud_notes(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<unkai_core::models::Note>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::notes::sync_nextcloud_notes(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn sync_nextcloud_task_lists(
    nc_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<TaskList>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::sync_nextcloud_task_lists(nc_id, &h.ctx.cache).await
}

#[tauri::command]
async fn sync_nextcloud_tasks(
    nc_id: String,
    list_id: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Vec<Task>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::sync_nextcloud_tasks(nc_id, list_id, &h.ctx.cache).await
}

#[tauri::command]
async fn test_connection(
    host: String,
    port: u16,
    username: String,
    password: String,
    trusted_certs: Option<Vec<unkai_core::models::TrustedCert>>,
) -> Result<String, UnkaiError> {
    cmds::accounts::test_connection(host, port, username, password, trusted_certs).await
}

#[tauri::command]
async fn test_jmap_connection(
    jmap_url: String,
    username: String,
    password: String,
) -> Result<String, UnkaiError> {
    cmds::accounts::test_jmap_connection(jmap_url, username, password).await
}

#[tauri::command]
async fn tombstone_draft_for_expunge(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::compose::tombstone_draft_for_expunge(account_id, folder, uid, &h.ctx.cache).await
}

#[tauri::command]
async fn try_auto_decrypt_message(
    account_id: String,
    folder: String,
    uid: u32,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Option<Email>, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::crypto::try_auto_decrypt_message(account_id, folder, uid, &h.ctx.cache).await
}

#[tauri::command]
fn unlock_with_passphrase(
    passphrase: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::unlock_with_passphrase(passphrase, &h.ctx.cache, &h.ctx.profile.id)
}

#[tauri::command]
fn unlock_with_prf(
    credential_id_b64: String,
    prf_output_b64: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::unlock_with_prf(
        credential_id_b64,
        prf_output_b64,
        &h.ctx.cache,
        &h.ctx.profile.id,
    )
}

#[tauri::command]
fn update_account(
    account: Account,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::accounts::update_account(account, &h.ctx.cache, &h.sync_notify)
}

#[tauri::command]
async fn update_app_settings(
    new_settings: AppSettings,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::settings::update_app_settings(
        new_settings,
        &h.ctx.settings,
        &h.sync_notify,
        &h.mcp,
        &h.ctx.profile,
    )
    .await
}

#[tauri::command]
async fn update_calendar_event(
    event_id: String,
    input: CalendarEventInput,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<CalendarEvent, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::update_calendar_event(event_id, input, &h.ctx.cache, h.ctx.ui.as_ref()).await
}

#[tauri::command]
async fn update_contact(
    contact_id: String,
    input: ContactInput,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Contact, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::update_contact(contact_id, input, &h.ctx.cache).await
}

#[tauri::command]
async fn update_contact_group(
    group_id: String,
    display_name: Option<String>,
    member_uids: Option<Vec<String>>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<ContactGroupView, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::contacts::update_contact_group(group_id, display_name, member_uids, &h.ctx.cache).await
}

#[tauri::command]
fn update_nextcloud_account_trusted_certs(
    nc_id: String,
    trusted_certs: Vec<unkai_core::models::TrustedCert>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<NextcloudAccount, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::update_nextcloud_account_trusted_certs(nc_id, trusted_certs, &h.ctx.cache)
}

#[tauri::command]
async fn update_nextcloud_calendar(
    calendar_id: String,
    display_name: Option<String>,
    color: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::calendar::update_nextcloud_calendar(calendar_id, display_name, color, &h.ctx.cache).await
}

#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
#[tauri::command]
async fn update_nextcloud_note(
    nc_id: String,
    note_id: u64,
    etag: String,
    title: Option<String>,
    content: Option<String>,
    category: Option<String>,
    favorite: Option<bool>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<unkai_core::models::Note, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::notes::update_nextcloud_note(
        nc_id,
        note_id,
        etag,
        title,
        content,
        category,
        favorite,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
async fn update_nextcloud_share(
    nc_id: String,
    share_id: String,
    password: Option<String>,
    permissions: Option<u8>,
    expire_date: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::update_nextcloud_share(
        nc_id,
        share_id,
        password,
        permissions,
        expire_date,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
async fn update_nextcloud_share_label(
    nc_id: String,
    share_id: String,
    label: String,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<(), UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::update_nextcloud_share_label(nc_id, share_id, label, &h.ctx.cache).await
}

#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
#[tauri::command]
async fn update_nextcloud_task(
    nc_id: String,
    list_id: String,
    uid: String,
    etag: String,
    summary: Option<String>,
    description: Option<String>,
    status: Option<String>,
    priority: Option<u8>,
    due_unix: Option<i64>,
    due_tz: Option<String>,
    clear_due: Option<bool>,
    completed_unix: Option<i64>,
    clear_completed: Option<bool>,
    url: Option<String>,
    categories: Option<Vec<String>>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<Task, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::tasks::update_nextcloud_task(
        nc_id,
        list_id,
        uid,
        etag,
        summary,
        description,
        status,
        priority,
        due_unix,
        due_tz,
        clear_due,
        completed_unix,
        clear_completed,
        url,
        categories,
        &h.ctx.cache,
    )
    .await
}

#[tauri::command]
fn update_profile(
    id: String,
    name: Option<String>,
    icon: Option<ProfileIcon>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<ProfileMeta, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::profiles::update_profile(h.ctx.ui.as_ref(), id, name, icon, reg.paths())
}

#[tauri::command]
async fn upload_to_nextcloud(
    nc_id: String,
    path: String,
    data: Vec<u8>,
    content_type: Option<String>,
    window: tauri::Window,
    reg: State<'_, ProfileRegistry>,
) -> Result<String, UnkaiError> {
    let h = profile_ctx(&window, &reg)?;
    cmds::nextcloud::upload_to_nextcloud(nc_id, path, data, content_type, &h.ctx.cache).await
}

// ── App entry point ─────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt::init();

    // Pick up the path the OS handed us if Unkai was invoked as
    // an `.ics` / `.eml` file handler (#254).  Capturing here —
    // before the Tauri builder runs — means the slot is populated
    // by the time the frontend's `take_pending_file_to_open`
    // ping arrives on first paint.
    capture_launch_file_arg();

    // Same idea for `mailto:` URLs (#294).  On Windows the OS
    // hands the URL through argv, and registering as the default
    // mailto handler at the OS level only takes effect after the
    // deep-link plugin's first run — we still want a cold-start
    // launch with a mailto in argv to land in Compose, so the
    // argv-scan path stays independent of the plugin's runtime
    // registration.
    capture_launch_mailto_arg();

    // Resolve the profile storage layout (#531).  `ensure_registry`
    // creates the registry on first run and migrates a pre-profile
    // flat install into `profiles/<id>/` — including the keychain
    // master key — strictly BEFORE the DB is opened (SQLCipher files
    // must never move while a pool holds them).  Failures here are
    // fatal: booting past a half-migrated layout could open (and
    // wipe-recreate) the wrong database.
    let profile_paths =
        unkai_store::ProfilePaths::from_config_dir().expect("cannot resolve config directory");
    let mut registry = unkai_store::profiles::ensure_registry(&profile_paths)
        .expect("failed to initialise profile registry");
    let profile = registry
        .startup_profile()
        .expect("profile registry is empty")
        .clone();
    if let Err(e) =
        unkai_store::profiles::touch_last_used(&profile_paths, &mut registry, &profile.id)
    {
        tracing::warn!("could not update profile last-used bookkeeping: {e}");
    }
    // MCP tokens are keyed per profile (#533).  A pre-profile
    // install left its token under the singleton keychain entry —
    // migrate it, but only while the registry holds exactly one
    // profile, so the target is unambiguous (chunks 3+ are what
    // make a second profile creatable, and by then every token is
    // per-profile).
    if registry.profiles.len() == 1
        && let Err(e) = credentials::migrate_legacy_mcp_token(&profile.id)
    {
        tracing::warn!("MCP token migration failed (token stays on the legacy entry): {e}");
    }

    // Captured for `.setup()`'s startup fan-out (#535): in `All`
    // mode every profile gets a window at boot, with the resolved
    // startup profile as the primary one in the static "main"
    // window.  `Fixed` / `LastUsed` open only that primary.
    let startup_mode = registry.startup.clone();
    let secondary_profile_ids: Vec<String> = registry
        .profiles
        .iter()
        .map(|p| p.id.clone())
        .filter(|id| *id != profile.id)
        .collect();

    tauri::Builder::default()
        // single-instance MUST come before any plugin that cares
        // about second-launch argv (here: deep-link).  With the
        // `deep-link` feature on, the plugin's callback routes the
        // forwarded argv through deep-link's own dispatcher, so
        // any `mailto:` URL hits the same `on_open_url` listener
        // whether it came from the fresh-launch or
        // second-launch path.  We still surface the window in the
        // callback so a mailto click from another app raises
        // Unkai to the foreground even before the URL hops
        // through deep-link.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            tracing::debug!("single-instance argv received: {argv:?}");
            // Raise first so the mailto (if any) lands in a window
            // the user is looking at.  #535: "the" window is the
            // most recently focused profile window, and the mailto
            // event targets exactly that window — a broadcast
            // would open a compose in every profile's shell.
            // (Full profile-aware external-open routing is chunk
            // 5; here a second launch simply must not regress.)
            let raised = windows::show_primary_window(app);
            for arg in &argv {
                if arg.to_lowercase().starts_with("mailto:") {
                    // Buffer + targeted emit, same shape as the
                    // deep-link path below: the buffer covers a
                    // window whose listeners aren't up yet, the
                    // frontend listener drains the buffer alongside
                    // the event so nothing is delivered twice.
                    buffer_mailto_url(arg);
                    if let Ok(r) = &raised
                        && !r.created
                        && let Err(e) = app.emit_to(r.window.label(), "unkai://mailto", arg.clone())
                    {
                        tracing::warn!("emit single-instance mailto failed: {e}");
                    }
                }
            }
            if let Err(e) = raised {
                tracing::warn!("single-instance window raise failed: {e}");
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        // #131 follow-up: cross-platform "launch on login".
        // The plugin registers an XDG autostart entry on Linux,
        // a LaunchAgent plist on macOS, and an HKCU\…\Run
        // registry value on Windows.  No launcher args — the
        // binary's default entry point is what we want
        // autostarted.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // The multi-profile runtime (#533).  Per-profile state
        // (cache, settings, MCP server, sync plumbing) lives in
        // ProfileHandles inside this registry — the only managed
        // state left outside it is genuinely machine-global.
        // Starts empty; the startup profile's context is built and
        // inserted in `.setup()` once the notifier's `AppHandle`
        // exists.
        .manage(ProfileRegistry::new(
            profile_paths.clone(),
            profile.id.clone(),
        ))
        .manage::<SystemFontsCache>(Arc::new(RwLock::new(Vec::new())))
        .register_uri_scheme_protocol("contact-photo", contact_photo_protocol)
        .register_uri_scheme_protocol("unkai-logo", logo_protocol)
        // Window lifecycle ↔ profile registry (#533/#535).  One
        // handler for every window — primary or popout — so none
        // of this logic is re-installed per window:
        //
        //   * Destroyed → drop the label→profile mapping (labels
        //     are reusable, and switch-in-place remaps them).
        //     Closing a profile's LAST window deliberately keeps
        //     its runtime context and background loops running —
        //     sync continues and the tray keeps the aggregate
        //     badge.  (Native new-mail toasts do NOT continue: the
        //     frontend raises them, so a window-less profile only
        //     badges the tray until a window reopens — a known
        //     #535 limitation, candidate for chunk 5.)  Only
        //     `delete_profile` and app exit tear contexts down.
        //   * CloseRequested on the static "main" window →
        //     minimize to tray when the window's *current* profile
        //     says so.  Resolved through the registry at event
        //     time, never a captured settings Arc — a
        //     switch-in-place must flip the behaviour with the
        //     profile.  `profile-*` windows always destroy on
        //     close (see the arm below for why).
        //   * Focused on a primary window → bookmark it as the
        //     window tray clicks / second launches / deep links
        //     raise, and keep the profile `last_used` order fresh.
        .on_window_event(|window, event| {
            let Some(reg) = window.app_handle().try_state::<ProfileRegistry>() else {
                return;
            };
            match event {
                WindowEvent::Destroyed => {
                    reg.unmap_window(window.label());
                }
                WindowEvent::CloseRequested { api, .. } => {
                    // Minimize-to-tray applies to the static
                    // "main" window ONLY.  A hidden `profile-*`
                    // window would keep its label→profile mapping
                    // forever (Destroyed never fires), which makes
                    // its profile permanently undeletable — the
                    // refusal says "close its window first" about
                    // a window the user already closed.  Closing a
                    // profile-* window destroys it; the profile's
                    // context keeps syncing and the tray raises or
                    // re-creates a window on demand.
                    if window.label() == "main"
                        && let Ok(h) = reg.handle_for_label(window.label())
                    {
                        // `blocking_read` is safe here: window
                        // events fire off the async runtime.
                        let should_hide = h.ctx.settings.blocking_read().minimize_to_tray;
                        if should_hide {
                            api.prevent_close();
                            let _ = window.hide();
                        }
                    }
                }
                WindowEvent::Focused(true) if windows::is_primary_label(window.label()) => {
                    let profile_id = reg.profile_for_label(window.label());
                    // Persist `last_used` only when the focused
                    // PROFILE changed — not on every focus flip
                    // between windows of the same profile.
                    if reg.note_focused(window.label(), &profile_id) {
                        windows::persist_last_used(reg.paths().clone(), profile_id);
                    }
                }
                _ => {}
            }
        })
        .setup(move |app| {
            // Windows toast attribution.  Without an explicit
            // AppUserModelID the OS falls back to the launching
            // process's AUMID — for `cargo tauri dev` that's the
            // shell (PowerShell, cmd, Git Bash), which is what
            // appears as the toast's source.  Setting our own AUMID
            // here makes notifications attribute to "Unkai Mail"
            // in both dev and bundled builds.  The display-name +
            // icon come from a Start-Menu shortcut the installer
            // registers with this same AUMID; in dev the toast
            // shows the AUMID itself, which is still better than
            // "PowerShell".
            #[cfg(windows)]
            set_app_user_model_id();

            // ── `mailto:` deep-link wiring (#294) ──────────────
            //
            // Three things happen here:
            //
            //   1. Register `mailto` as a handled URI scheme at
            //      runtime.  The bundle config registers it at
            //      install time, but `register()` is what writes
            //      the per-user registry keys on Windows (and the
            //      per-user `.desktop` association on Linux) for
            //      dev / portable launches that never run an
            //      installer.  Idempotent — safe to call every
            //      boot.
            //   2. Drain `get_current()` into our cold-start
            //      buffer.  Tauri exposes the URL the OS used to
            //      spawn us here; without this, a fresh launch
            //      from a mailto link delivers the URL *before*
            //      the frontend has registered an event listener
            //      and we'd silently drop it.
            //   3. Subscribe to `on_open_url` for any live URL
            //      that arrives after the webview is up.  We emit
            //      a `unkai://mailto` Tauri event with the raw
            //      URL; the frontend parses it with the same
            //      `parseMailtoUrl` helper the in-app body
            //      handler uses and opens Compose pre-filled.
            //
            // The deep-link plugin is `cfg(desktop)`-gated on
            // mobile by Tauri itself, so wrapping our calls in
            // `cfg!(desktop)` would be redundant — on iOS /
            // Android the plugin trait isn't even present.
            use tauri_plugin_deep_link::DeepLinkExt;
            let dl = app.deep_link();
            if let Err(e) = dl.register("mailto") {
                tracing::warn!(
                    "deep-link mailto registration failed (OS will not route mailto links here \
                     until next launch / installer run): {e}"
                );
            }
            match dl.get_current() {
                Ok(Some(urls)) => {
                    for u in urls {
                        let s = u.to_string();
                        if s.to_lowercase().starts_with("mailto:") {
                            buffer_mailto_url(&s);
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("deep-link get_current failed: {e}"),
            }
            let handle_for_links = app.handle().clone();
            dl.on_open_url(move |event| {
                for url in event.urls() {
                    let s = url.to_string();
                    if !s.to_lowercase().starts_with("mailto:") {
                        continue;
                    }
                    // Buffer + emit covers the race where the OS
                    // delivers the URL after `setup` returns but
                    // before App.svelte's mount has wired up the
                    // listener: the buffer catches it and the
                    // frontend's `take_pending_mailto_urls` poll
                    // drains it on mount.  #535: the emit targets
                    // the most recently focused profile window
                    // (a broadcast would open a compose in every
                    // profile's shell), and the frontend listener
                    // drains the buffer alongside the event so the
                    // duplicate entry can't be replayed by a later
                    // profile window's mount.
                    buffer_mailto_url(&s);
                    match windows::show_primary_window(&handle_for_links) {
                        Ok(raised) if !raised.created => {
                            if let Err(e) = handle_for_links.emit_to(
                                raised.window.label(),
                                "unkai://mailto",
                                s.clone(),
                            ) {
                                tracing::warn!("emit deep-link mailto failed: {e}");
                            }
                        }
                        // A freshly created window drains the
                        // buffer on mount — no emit needed.
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("deep-link window raise failed: {e}");
                        }
                    }
                }
            });

            // Drop the app icon onto disk once and stash its path
            // in managed state so the JS layer can pass it to
            // `sendNotification`.  Without this, libnotify on Linux
            // (and macOS' NSUserNotification) fall back to a
            // generic icon next to the toast in dev builds.
            // Always manage the state, even on failure, so commands
            // taking `State<'_, NotificationIconPath>` always extract
            // (an empty path signals "no icon known").
            let icon_path = install_notification_icon()
                .inspect_err(|e| tracing::warn!("install_notification_icon failed: {e}"))
                .unwrap_or_default();
            app.manage(NotificationIconPath(icon_path));

            // ── Startup profile context (#476/#533) ─────────────
            //
            // Open the startup profile: cache, settings, MCP
            // server, notifier, and its set of background loops —
            // all bundled into a ProfileHandle by the registry's
            // builder.  Done here (not before the builder) because
            // the `TauriNotifier` inside needs an `AppHandle`.
            //
            // The "main" window maps to the startup profile BEFORE
            // the handle is built, so anything the freshly-spawned
            // loops emit already resolves to a window; chunk 4
            // generalises window creation to `profile-<id>` labels
            // registered the same way.
            //
            // A cache-open failure is fatal: without the cache the
            // write-through path is broken, and the user would
            // silently lose offline capability.
            let profile_handle = {
                let reg = app.state::<ProfileRegistry>();
                let startup_id = reg.startup_profile_id().to_string();
                reg.map_window("main", &startup_id);
                // Seed the focus bookmark so tray clicks / second
                // launches have a target even when the app boots
                // straight into the tray and no focus event ever
                // fires (#535).
                reg.note_focused("main", &startup_id);
                let handle = registry::build_profile_handle(app.handle(), &startup_id, reg.paths())
                    .map_err(|e| format!("failed to open the startup profile: {e}"))?;
                reg.insert_profile(&startup_id, handle.clone());
                handle
            };
            let ctx = profile_handle.ctx.clone();

            // Warm the system-fonts cache off the main thread so
            // the first compose-toolbar font-dropdown open is
            // instant.  Two-tier strategy:
            //
            //   1. Compute a cheap fingerprint of the system font
            //      directories (recursive dir-mtime hash).
            //   2. If a JSON cache exists at the same fingerprint,
            //      load it — saves font-kit's catalogue walk
            //      entirely on every launch where the user hasn't
            //      installed or removed a font.
            //   3. Otherwise run font-kit fresh and persist the
            //      result so the next launch hits the fast path.
            //
            // Runs on a plain OS thread because Tauri's setup
            // callback fires before the async runtime is mounted
            // — calling tokio here would panic with "no reactor
            // running".  We park on the tokio RwLock via
            // `blocking_write`; the lock is uncontended at startup
            // so this is effectively immediate.
            let fonts_cache = app.state::<SystemFontsCache>().inner().clone();
            std::thread::spawn(move || {
                let fingerprint = compute_font_fingerprint();
                if let Some(disk) = load_font_cache_file()
                    && disk.fingerprint == fingerprint
                    && !disk.fonts.is_empty()
                {
                    let count = disk.fonts.len();
                    *fonts_cache.blocking_write() = disk.fonts;
                    tracing::info!("system fonts loaded from disk cache: {count} families");
                    return;
                }
                let list = enumerate_system_fonts();
                let count = list.len();
                save_font_cache_file(&FontCacheFile {
                    fingerprint,
                    fonts: list.clone(),
                });
                *fonts_cache.blocking_write() = list;
                tracing::info!("system fonts enumerated fresh: {count} families");
            });

            // ── Tray menu + icon ────────────────────────────────
            //
            // Built inside `setup` (not a command) so we have `&mut App`
            // and can register the tray lifecycle against the Tauri
            // event loop directly.
            let handle = app.handle().clone();
            let menu = Menu::with_items(
                &handle,
                &[
                    &MenuItem::with_id(&handle, "open", "Open Unkai", true, None::<&str>)?,
                    &MenuItem::with_id(&handle, "check", "Check Mail Now", true, None::<&str>)?,
                    &MenuItem::with_id(&handle, "compose", "Compose", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(&handle)?,
                    &MenuItem::with_id(&handle, "quit", "Quit Unkai", true, None::<&str>)?,
                ],
            )?;

            // Honour the startup profile's saved logo style at
            // boot.  Falls back to "storm" if decoding fails for
            // any reason — keeps the tray from coming up blank on
            // a malformed settings file.
            let chosen_style = {
                let s = futures::executor::block_on(ctx.settings.read());
                s.logo_style.clone()
            };
            let style_bytes = logo_bytes_for(&chosen_style);
            let initial_bitmap = decode_logo_png(style_bytes).unwrap_or_else(|e| {
                tracing::warn!(
                    "logo style '{chosen_style}' failed to decode ({e}); \
                     falling back to storm"
                );
                decode_logo_png(logo_assets::STORM).expect("storm logo PNG must always decode")
            });

            // Reflect the chosen style on the main window — the
            // titlebar icon and (on Windows) the taskbar entry both
            // pick up `set_icon`.
            if let Some(win) = app.get_webview_window("main")
                && let Ok(img) = tauri::image::Image::from_bytes(style_bytes)
                && let Err(e) = win.set_icon(img)
            {
                tracing::warn!("failed to apply window icon at boot: {e}");
            }

            // Tauri's `Image::from_bytes` decodes into owned RGBA
            // (`Image<'static>`), which is what `TrayIconBuilder`
            // wants.  Using `from_bytes` again here (instead of
            // `Image::new(&initial_bitmap.rgba, ...)`) sidesteps a
            // borrow-vs-move conflict where we want to *also*
            // hand `initial_bitmap` to the managed-state stash.
            let tray_icon = tauri::image::Image::from_bytes(style_bytes)
                .map_err(|e| UnkaiError::Other(format!("decode tray icon: {e}")))?;

            // Stash the base RGBA in managed state so the badge
            // renderer (and `set_logo_style`) can re-composite
            // without re-reading the PNG on every unread-count
            // change or style swap.
            app.manage(TrayBaseIcon(std::sync::Mutex::new(initial_bitmap)));

            let _tray = TrayIconBuilder::with_id("unkai-main")
                .icon(tray_icon)
                .tooltip("Unkai Mail")
                .menu(&menu)
                // Windows: without this, left-click auto-pops the menu
                // and our click-handler never fires. We want left-click
                // to show the window, right-click to show the menu.
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => {
                        // #535: raises the most recently focused
                        // profile window (the tray is shared
                        // chrome with no profile of its own).
                        if let Err(e) = windows::show_primary_window(app) {
                            tracing::warn!("tray open failed: {e}");
                        }
                    }
                    "check" => {
                        // The tray is shared chrome — "Check Mail
                        // Now" fans out across every open profile.
                        for h in app.state::<ProfileRegistry>().handles() {
                            let ctx = h.ctx.clone();
                            tauri::async_runtime::spawn(async move {
                                if let Err(e) = cmds::mail::check_mail_now_inner(&ctx).await {
                                    tracing::warn!("tray check_mail_now failed: {e}");
                                }
                            });
                        }
                    }
                    "compose" => match windows::show_primary_window(app) {
                        // Target the raised window — a broadcast
                        // would open a compose in every profile's
                        // shell (#535).  A freshly created window
                        // has no listeners yet; the user still
                        // lands in the shell and can compose from
                        // there.
                        Ok(raised) if !raised.created => {
                            if let Err(e) = app.emit_to(raised.window.label(), "open-compose", ()) {
                                tracing::warn!("failed to emit open-compose: {e}");
                            }
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("tray compose open failed: {e}"),
                    },
                    "quit" => app.exit(0),
                    other => tracing::debug!("unknown tray menu id: {other}"),
                })
                .on_tray_icon_event(|tray, event| {
                    // Single left-click (button up) opens the window.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                        && let Err(e) = windows::show_primary_window(tray.app_handle())
                    {
                        tracing::warn!("tray left-click show failed: {e}");
                    }
                })
                .build(app)?;

            // ── First show ──────────────────────────────────────
            //
            // Close-to-tray lives in the builder-level
            // `.on_window_event` handler since #535 — resolved per
            // window through the registry, so `profile-*` windows
            // and switched-in-place windows behave by their own
            // profile's preference.
            //
            // The main window starts hidden (`visible: false` in
            // tauri.conf.json) so we don't paint it with the
            // bundled storm icon for a frame before the user's
            // chosen logo style is applied above.  Now that the
            // icon is in place, decide whether to show it:
            //   - `start_minimized` true → leave it hidden, app
            //     boots straight into the tray.
            //   - otherwise → show the window with the correct
            //     icon already painted in the titlebar / taskbar.
            if let Some(main_window) = app.get_webview_window("main") {
                let should_hide_on_start = ctx.settings.blocking_read().start_minimized;
                if !should_hide_on_start {
                    let _ = main_window.show();
                    // #470 — this `visible: false` → `show()` transition
                    // is where the Linux titlebar loses its buttons, so
                    // the decoration gets rebuilt right after the map.
                    rebuild_decoration_input_region(&main_window);
                }
            } else {
                tracing::warn!("main window not found at setup time");
            }

            // ── Startup fan-out (#535) ──────────────────────────
            //
            // `All` mode: one window per profile.  The resolved
            // startup profile already owns the static "main"
            // window above; every other profile gets its context
            // built and a `profile-*` window created, each
            // honouring its own `start_minimized` preference and
            // none stealing focus from the startup window.
            // Spawned so boot isn't gated on N SQLCipher key
            // derivations, and the builds run on the blocking pool
            // — they're real synchronous IO.  Failures are
            // per-profile and non-fatal: one broken profile must
            // not stop the rest of the app booting.
            if matches!(startup_mode, StartupMode::All) {
                let fan_out_app = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    for id in secondary_profile_ids {
                        let build_app = fan_out_app.clone();
                        let build_id = id.clone();
                        let built = tauri::async_runtime::spawn_blocking(move || {
                            windows::ensure_profile_context(&build_app, &build_id)
                        })
                        .await;
                        match built {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    "startup fan-out: profile '{id}' failed to open: {e}"
                                );
                                continue;
                            }
                            Err(e) => {
                                tracing::warn!("startup fan-out: build task for '{id}' died: {e}");
                                continue;
                            }
                        }
                        let show = fan_out_app
                            .state::<ProfileRegistry>()
                            .context_for(&id)
                            .and_then(|h| {
                                h.ctx.settings.try_read().map(|s| !s.start_minimized).ok()
                            })
                            .unwrap_or(true);
                        if let Err(e) =
                            windows::create_profile_window(&fan_out_app, &id, show, false)
                        {
                            tracing::warn!("startup fan-out: profile '{id}' window failed: {e}");
                        }
                    }
                });
            }

            // Paint the initial badge from whatever's already in the
            // cache so the tray + taskbar reflect unread count from
            // the moment the app finishes booting (not only after the
            // first sync tick).
            refresh_unread_badge(&ctx.cache, ctx.ui.as_ref());

            // The startup profile's background loops (sync, reminders,
            // prerender, settings sync, URLhaus refresh) and its MCP
            // boot reconcile were spawned by `build_profile_handle`
            // above — one set per profile context (#533).

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_notification_icon_path,
            send_native_notification,
            get_accounts,
            add_account,
            remove_account,
            update_account,
            set_account_password,
            set_folder_icon,
            discover_account_settings,
            list_provider_presets,
            add_dav_account,
            add_local_dav_account,
            probe_server_certificate,
            test_connection,
            fetch_envelopes,
            fetch_unified_envelopes,
            fetch_unified_special_envelopes,
            get_unified_special_cached_envelopes,
            fetch_older_envelopes,
            fetch_older_unified_envelopes,
            fetch_message,
            download_email_attachment,
            download_decrypted_attachment,
            fetch_inline_images,
            put_attachment_preview,
            get_attachment_previews,
            download_calendar_from_message,
            fetch_folders,
            create_folder,
            delete_folder,
            rename_folder,
            clear_folder,
            mark_folder_read,
            mark_as_read,
            set_message_read,
            set_message_flagged,
            set_message_pinned,
            set_message_priority,
            set_message_reminder,
            respond_mdn_request,
            get_receipt_status,
            send_email,
            list_outbox,
            list_all_outbox,
            count_outbox,
            count_outbox_by_account,
            retry_outbox_entry,
            retry_outbox_entry_with_passphrase,
            delete_outbox_entry,
            edit_outbox_entry,
            save_draft,
            tombstone_draft_for_expunge,
            expunge_draft_after_send,
            delete_message,
            archive_message,
            archive_messages,
            move_message,
            move_messages,
            get_cached_envelopes,
            get_unified_cached_envelopes,
            get_envelopes_by_thread,
            get_cached_message,
            get_cached_folders,
            test_jmap_connection,
            detect_jmap,
            search_emails,
            search_imap_server,
            search_imap_server_older,
            start_nextcloud_login,
            poll_nextcloud_login,
            get_nextcloud_accounts,
            refresh_nextcloud_capabilities,
            get_nextcloud_user_email,
            remove_nextcloud_account,
            update_nextcloud_account_trusted_certs,
            open_url,
            list_nextcloud_files,
            download_nextcloud_file,
            nextcloud_file_preview,
            create_nextcloud_share,
            update_nextcloud_share_label,
            update_nextcloud_share,
            list_nextcloud_shares,
            delete_nextcloud_share,
            create_nextcloud_directory,
            list_talk_rooms,
            create_talk_room,
            set_talk_room_public,
            find_nextcloud_user_by_email,
            add_talk_participants,
            delete_talk_room,
            add_talk_participant,
            rename_talk_room,
            list_nextcloud_notes,
            sync_nextcloud_notes,
            get_nextcloud_note,
            create_nextcloud_note,
            update_nextcloud_note,
            delete_nextcloud_note,
            list_nextcloud_task_lists,
            sync_nextcloud_task_lists,
            list_nextcloud_tasks,
            sync_nextcloud_tasks,
            create_nextcloud_task,
            update_nextcloud_task,
            delete_nextcloud_task,
            create_nextcloud_task_from_mail,
            set_nextcloud_task_list_hidden,
            set_nextcloud_task_list_muted,
            get_tasks_sync_status,
            upload_to_nextcloud,
            office_open_attachment,
            office_close_attachment,
            office_sweep_temp,
            pdf_open_attachment,
            pdf_close_attachment,
            print_attachment,
            save_attachment_as,
            sync_nextcloud_contacts,
            get_contacts_sync_status,
            get_calendars_sync_status,
            get_contacts,
            search_contacts,
            get_contact_photo,
            create_contact,
            update_contact,
            delete_contact,
            import_calendar_file,
            import_contacts_file,
            list_contact_groups,
            create_contact_group,
            update_contact_group,
            delete_contact_group,
            set_contact_group_hidden,
            set_contact_group_emoji,
            list_nextcloud_groups,
            list_contact_categories,
            set_category_use_as_mailing_list,
            add_contact_to_category,
            remove_contact_from_category,
            rename_contact_category,
            delete_contact_category,
            list_mailing_lists,
            set_mailing_list_hidden,
            set_mailing_list_emoji,
            rename_mailing_list,
            list_nextcloud_addressbooks,
            list_nextcloud_calendars,
            sync_nextcloud_calendars,
            sync_calendar_by_id,
            get_cached_calendars,
            create_nextcloud_calendar,
            update_nextcloud_calendar,
            delete_nextcloud_calendar,
            set_nextcloud_calendar_hidden,
            set_nextcloud_calendar_muted,
            get_cached_events,
            create_calendar_event,
            parse_event_invite,
            respond_to_invite,
            rsvp_existing_event,
            get_rsvp_response,
            get_event_partstat_for_user,
            update_calendar_event,
            delete_calendar_event,
            get_attendee_availability,
            geocode_search,
            detect_nc_maps,
            dismiss_cancelled_event,
            is_event_in_calendar,
            record_cancelled_invite,
            is_invite_cancelled,
            // Issue #16: tray + notifications + preferences
            get_app_settings,
            list_system_fonts,
            fido_status,
            fido_generate_salt,
            fido_enroll,
            fido_enroll_passphrase,
            fido_verify_passphrase,
            fido_verify_prf,
            fido_remove,
            database_status,
            unlock_with_passphrase,
            unlock_with_prf,
            enable_fido_only_mode,
            disable_fido_only_mode,
            get_wipe_policy,
            set_wipe_policy,
            update_app_settings,
            // Issue #168: settings backup & sync (#477: dialog-paired).
            export_settings_bundle,
            import_settings_bundle,
            mcp_generate_token,
            mcp_revoke_token,
            mcp_token_status,
            mcp_server_status,
            mcp_list_tools,
            get_settings_sync_state,
            set_settings_sync_target,
            notify_settings_changed,
            nc_probe_settings_bundle,
            nc_restore_settings_bundle,
            // Issue #165: URLhaus link safety.
            check_urls,
            debug_link_check,
            get_link_check_status,
            refresh_urlhaus_now,
            set_logo_style,
            import_custom_theme,
            remove_custom_theme,
            check_mail_now,
            dismiss_event_reminder,
            snooze_event_reminder,
            get_total_unread,
            get_unread_counts_by_account,
            quit_app,
            restart_app,
            // #254 — file-association entry points
            take_pending_file_to_open,
            parse_eml_file,
            parse_eml_file_inline_images,
            parse_ics_file,
            open_default_apps_settings,
            // #294 — OS-level mailto handler cold-start drain
            take_pending_mailto_urls,
            // #57 — end-to-end mail encryption: key management
            pgp_import_private_key,
            pgp_remove_private_key,
            pgp_get_account_key_status,
            pgp_import_public_key,
            pgp_remove_public_key,
            pgp_list_public_keys,
            pgp_get_keys_for_email,
            // #341 — per-account "Unlock automatically" opt-in
            pgp_enable_unlock_automatically,
            pgp_disable_unlock_automatically,
            pgp_has_unlock_automatically,
            // #338 — S/MIME (X.509) certificate storage + identity
            smime_import_pkcs12,
            smime_remove_private_cert,
            smime_get_account_cert_status,
            smime_enable_unlock_automatically,
            smime_disable_unlock_automatically,
            smime_has_unlock_automatically,
            smime_import_public_cert,
            smime_remove_public_cert,
            smime_list_public_certs,
            smime_get_certs_for_email,
            // #57 — on-demand decrypt for inbound PGP/MIME messages
            decrypt_message,
            try_auto_decrypt_message,
            // #534 — profile management
            list_profiles,
            get_current_profile,
            create_profile,
            update_profile,
            delete_profile,
            get_startup_mode,
            set_startup_mode,
            // #535 — profile windows
            open_profile_window,
            switch_window_profile,
            register_popout_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Unkai");
}

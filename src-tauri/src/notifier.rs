//! The desktop implementation of [`UiNotifier`] (#476).
//!
//! `unkai-commands` decides *when* the user should be told something
//! and *what* to tell them; this file is the only place that knows the
//! answer is "emit a Tauri event" / "repaint the tray icon".  Swapping
//! the shell means writing another one of these and nothing else.
//!
//! Every method is best-effort: a failed emit is logged and dropped
//! rather than propagated, because none of the ~40 call sites can do
//! anything useful with the failure.  The two that do return a
//! `Result` are documented on the trait.

use std::collections::HashMap;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use unkai_commands::notify::{
    CalendarsUpdatedPayload, EventReminderPayload, MailFlagsUpdatedPayload, MessageReminderPayload,
    NewMailPayload, OutboxUpdatedPayload, UiNotifier,
};
use unkai_core::UnkaiError;
use unkai_store::Cache;

use crate::badge;
use crate::tray::{TrayBaseIcon, decode_logo_png, logo_bytes_for};

/// Pushes application-layer notifications out over Tauri's IPC and
/// into the native chrome.
pub struct TauriNotifier {
    app: AppHandle,
}

impl TauriNotifier {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }

    /// One place for the "emit, log on failure, carry on" shape every
    /// push channel wants.
    fn push<P: Serialize + Clone>(&self, event: &str, payload: P) {
        if let Err(e) = self.app.emit(event, payload) {
            tracing::warn!("failed to emit {event} event: {e}");
        }
    }
}

impl UiNotifier for TauriNotifier {
    fn new_mail(&self, payload: &NewMailPayload) {
        self.push("new-mail", payload);
    }

    fn mail_flags_updated(&self, payload: &MailFlagsUpdatedPayload) {
        self.push("mail-flags-updated", payload);
    }

    fn outbox_updated(&self, payload: &OutboxUpdatedPayload) {
        self.push("outbox-updated", payload);
    }

    fn calendars_updated(&self, payload: &CalendarsUpdatedPayload) {
        self.push("calendars-updated", payload);
    }

    fn event_reminder(&self, payload: &EventReminderPayload) {
        self.push("event-reminder", payload);
    }

    fn message_reminder(&self, payload: &MessageReminderPayload) -> Result<(), UnkaiError> {
        self.app
            .emit("message-reminder", payload)
            .map_err(|e| UnkaiError::Other(format!("emit message-reminder: {e}")))
    }

    /// Repaint everything that carries the unread count: the tray icon
    /// (badge + tooltip), the Windows taskbar overlay, and the
    /// frontend event.
    fn unread_total_changed(&self, total: u32) {
        if let Some(tray) = self.app.tray_by_id("unkai-main") {
            let base = self.app.state::<TrayBaseIcon>();
            match base.0.lock() {
                Ok(guard) => {
                    let bitmap = guard.clone();
                    drop(guard);
                    let badged =
                        badge::render_tray_icon(&bitmap.rgba, bitmap.width, bitmap.height, total);
                    if let Err(e) = tray.set_icon(Some(badged)) {
                        tracing::warn!("failed to update tray icon: {e}");
                    }
                    let tip = if total == 0 {
                        "Unkai Mail".to_string()
                    } else {
                        format!("Unkai Mail — {total} unread")
                    };
                    let _ = tray.set_tooltip(Some(&tip));
                }
                // A poisoned lock loses the tray repaint, but the
                // frontend event below still goes out — the count the
                // user reads in the app stays correct either way.
                Err(e) => tracing::warn!("tray base lock poisoned: {e}"),
            }
        }

        // Windows-only: the taskbar overlay icon. macOS/Linux have no
        // direct equivalent — `set_overlay_icon` only exists behind
        // `#[cfg(windows)]`. We tried badging the window icon on those
        // platforms via `WebviewWindow::set_icon`, but on Linux that
        // sets the X11 `_NET_WM_ICON` atom — which most WMs (KDE,
        // XFCE, Cinnamon) use for both the taskbar entry AND the
        // title-bar icon. No way through Tauri to update one without
        // the other, and a badged title-bar icon looks out of place
        // sitting next to the window title. So on non-Windows we leave
        // the badge to the system tray icon alone.
        #[cfg(windows)]
        if let Some(win) = self.app.get_webview_window("main") {
            let overlay = badge::render_taskbar_overlay(total);
            if let Err(e) = win.set_overlay_icon(overlay) {
                tracing::warn!("failed to set taskbar overlay icon: {e}");
            }
        }

        self.push("unread-count-updated", total);
    }

    fn unread_by_account_changed(&self, by_account: &HashMap<String, u32>) {
        self.push("unread-count-by-account-updated", by_account);
    }

    fn custom_themes_changed(&self) {
        self.push("custom-themes-changed", ());
    }

    /// Swap the running app's icon (tray base bitmap + window /
    /// taskbar icon) to `style`.
    ///
    /// Note this only swaps icons that exist *while the app runs*; the
    /// `.exe` thumbnail Windows Explorer / macOS Finder shows for the
    /// installed binary is baked in at `cargo tauri build` time and
    /// can't change at runtime.
    fn apply_logo_style(&self, style: &str) -> Result<(), UnkaiError> {
        let bytes = logo_bytes_for(style);

        // Decode once up front so a bad slug fails before we touch any
        // running state.  `decode_logo_png` falls back to storm
        // internally if the slug is unknown, so this should always
        // succeed for reasonable inputs.
        let bitmap = decode_logo_png(bytes)?;

        // Swap the tray base bitmap so the next badge re-render uses
        // the new style.  Then trigger an immediate re-render so the
        // tray reflects the change without waiting for the next
        // unread-count tick.
        if let Some(tray_state) = self.app.try_state::<TrayBaseIcon>()
            && let Ok(mut guard) = tray_state.0.lock()
        {
            *guard = bitmap;
        }
        let total = self
            .app
            .state::<Cache>()
            .total_unread_count()
            .unwrap_or_default();
        self.unread_total_changed(total);

        // Update the main window's icon — Windows mirrors this into
        // the taskbar entry, macOS into the title bar, X11 into the
        // `_NET_WM_ICON` atom.
        if let Some(win) = self.app.get_webview_window("main")
            && let Ok(img) = tauri::image::Image::from_bytes(bytes)
            && let Err(e) = win.set_icon(img)
        {
            tracing::warn!("apply_logo_style: window set_icon failed: {e}");
        }
        Ok(())
    }
}

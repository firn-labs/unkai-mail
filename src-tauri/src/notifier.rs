//! The desktop implementation of [`UiNotifier`] (#476).
//!
//! `unkai-commands` decides *when* the user should be told something
//! and *what* to tell them; this file is the only place that knows the
//! answer is "emit a Tauri event" / "repaint the tray icon".  Swapping
//! the shell means writing another one of these and nothing else.
//!
//! One notifier exists per **profile** (#533): events target only the
//! owning profile's window(s) — a work-profile new-mail toast must not
//! fire in the private window — resolved through the registry's
//! window map.  The trait itself stays profile-blind; this impl knows
//! its profile.  (Until chunk 4 moves the frontend to window-targeted
//! listeners, plain `listen()` still receives every targeted emit —
//! the scoping below becomes load-bearing the moment that lands.)
//!
//! The system tray is the one deliberately *shared* surface: one
//! icon, badged with the aggregate unread count across all open
//! profiles (planning decision on #530).
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

use crate::badge;
use crate::registry::ProfileRegistry;
use crate::tray::{TrayBaseIcon, decode_logo_png, logo_bytes_for};

/// Pushes one profile's application-layer notifications out over
/// Tauri's IPC and into the native chrome.
pub struct TauriNotifier {
    app: AppHandle,
    profile_id: String,
}

impl TauriNotifier {
    pub fn new(app: AppHandle, profile_id: String) -> Self {
        Self { app, profile_id }
    }

    /// The window labels currently belonging to this notifier's
    /// profile.  Empty before the first window is mapped (or after
    /// the last closes) — then there is simply no one to tell.
    fn window_labels(&self) -> Vec<String> {
        match self.app.try_state::<ProfileRegistry>() {
            Some(reg) => reg.labels_for_profile(&self.profile_id),
            None => Vec::new(),
        }
    }

    /// One place for the "emit to this profile's windows, log on
    /// failure, carry on" shape every push channel wants.
    fn push<P: Serialize + Clone>(&self, event: &str, payload: P) {
        for label in self.window_labels() {
            if let Err(e) = self.app.emit_to(label.as_str(), event, payload.clone()) {
                tracing::warn!("failed to emit {event} event to '{label}': {e}");
            }
        }
    }

    /// Repaint the single shared tray icon (badge + tooltip) with
    /// the aggregate unread count across all profiles.
    fn repaint_tray(&self, aggregate: u32) {
        let Some(tray) = self.app.tray_by_id("unkai-main") else {
            return;
        };
        let base = self.app.state::<TrayBaseIcon>();
        match base.0.lock() {
            Ok(guard) => {
                let bitmap = guard.clone();
                drop(guard);
                let badged =
                    badge::render_tray_icon(&bitmap.rgba, bitmap.width, bitmap.height, aggregate);
                if let Err(e) = tray.set_icon(Some(badged)) {
                    tracing::warn!("failed to update tray icon: {e}");
                }
                let tip = if aggregate == 0 {
                    "Unkai Mail".to_string()
                } else {
                    format!("Unkai Mail — {aggregate} unread")
                };
                let _ = tray.set_tooltip(Some(&tip));
            }
            // A poisoned lock loses the tray repaint, but the
            // frontend event still goes out — the count the user
            // reads in the app stays correct either way.
            Err(e) => tracing::warn!("tray base lock poisoned: {e}"),
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
        for label in self.window_labels() {
            self.app
                .emit_to(label.as_str(), "message-reminder", payload)
                .map_err(|e| UnkaiError::Other(format!("emit message-reminder: {e}")))?;
        }
        Ok(())
    }

    /// Repaint everything that carries an unread count.  `total` is
    /// THIS profile's count: the profile's windows get it (event +
    /// Windows taskbar overlay), while the shared tray badges the
    /// aggregate across all profiles via the registry.
    fn unread_total_changed(&self, total: u32) {
        let aggregate = match self.app.try_state::<ProfileRegistry>() {
            Some(reg) => reg.record_unread(&self.profile_id, total),
            // No registry (shouldn't happen once setup ran) —
            // painting our own count beats painting nothing.
            None => total,
        };
        self.repaint_tray(aggregate);

        // Windows-only: the taskbar overlay icon on this profile's
        // windows, carrying the profile's own count — each window
        // represents one profile, so its badge shows that profile's
        // mail.  macOS/Linux have no per-window equivalent —
        // `set_overlay_icon` only exists behind `#[cfg(windows)]`,
        // and badging the window icon on those platforms also
        // repaints the title-bar icon (X11 `_NET_WM_ICON`), which
        // looks out of place.  There the tray badge stands alone.
        #[cfg(windows)]
        for label in self.window_labels() {
            if let Some(win) = self.app.get_webview_window(&label) {
                let overlay = badge::render_taskbar_overlay(total);
                if let Err(e) = win.set_overlay_icon(overlay) {
                    tracing::warn!("failed to set taskbar overlay icon: {e}");
                }
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

    /// Swap the running app's icon (tray base bitmap + this
    /// profile's window / taskbar icons) to `style`.
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
        // the new style, then repaint immediately with the current
        // aggregate so the tray reflects the change without waiting
        // for the next unread-count tick.
        if let Some(tray_state) = self.app.try_state::<TrayBaseIcon>()
            && let Ok(mut guard) = tray_state.0.lock()
        {
            *guard = bitmap;
        }
        let aggregate = match self.app.try_state::<ProfileRegistry>() {
            Some(reg) => reg.unread_sum(),
            None => 0,
        };
        self.repaint_tray(aggregate);

        // Update this profile's window icons — Windows mirrors this
        // into the taskbar entry, macOS into the title bar, X11 into
        // the `_NET_WM_ICON` atom.
        for label in self.window_labels() {
            if let Some(win) = self.app.get_webview_window(&label)
                && let Ok(img) = tauri::image::Image::from_bytes(bytes)
                && let Err(e) = win.set_icon(img)
            {
                tracing::warn!("apply_logo_style: window set_icon failed for '{label}': {e}");
            }
        }
        Ok(())
    }
}

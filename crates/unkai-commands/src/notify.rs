//! The one seam between the application layer and whatever shell is
//! driving it (#476).
//!
//! Everything in this crate is transport-agnostic, but plenty of it
//! still needs to *tell the user something happened* — new mail
//! landed, the outbox drained, a reminder came due.  Under Tauri that
//! was `AppHandle::emit("new-mail", …)`, which dragged a Tauri type
//! into every function that wanted to speak.  Here it is a trait
//! instead: the desktop crate implements `UiNotifier` on top of
//! `AppHandle`, a test implements it as a `Vec` of recorded calls,
//! and a future headless deployment implements it however it likes.
//!
//! Two groups of methods:
//!
//! * **push events** — the direct replacements for the old `emit`
//!   channels.  Each one carries the same payload struct the frontend
//!   already deserialises, so the wire format is unchanged.
//! * **native chrome** — tray badge, taskbar overlay, app icon.  These
//!   have no frontend event; they exist because "reflect the unread
//!   count in the UI" genuinely means painting a tray icon on desktop
//!   and may mean nothing at all elsewhere.
//!
//! Implementations are expected to be **best-effort and infallible**:
//! a dropped notification is a cosmetic loss (the user refreshes and
//! sees the truth), so the methods return `()` and swallow their own
//! transport errors rather than making 40 call sites handle a failure
//! they can do nothing about.  Two methods are deliberate exceptions —
//! `apply_logo_style` validates its input, and `message_reminder`
//! gates a cache write on the push having landed; both document why.

use std::collections::HashMap;

use serde::Serialize;
use unkai_core::UnkaiError;

/// How the application layer talks back to the UI.
pub trait UiNotifier: Send + Sync + 'static {
    /// New messages arrived in a folder during a background poll.
    fn new_mail(&self, payload: &NewMailPayload);

    /// Flags changed on messages in `folder` — the list should re-read
    /// the cache.
    fn mail_flags_updated(&self, payload: &MailFlagsUpdatedPayload);

    /// The outbox queue changed shape (enqueue / drain / delete).
    fn outbox_updated(&self, payload: &OutboxUpdatedPayload);

    /// A calendar's metadata changed (today: flipped to read-only after
    /// a CalDAV 403/404).
    fn calendars_updated(&self, payload: &CalendarsUpdatedPayload);

    /// An event reminder came due.
    fn event_reminder(&self, payload: &EventReminderPayload);

    /// A message reminder ("remind me about this mail") came due.
    ///
    /// Fallible, unlike its siblings: the caller only clears the row's
    /// `reminder_at` once the notification actually went out, so a
    /// failed push has to be reportable or a reminder is silently
    /// lost.  Retrying next tick (and risking a duplicate toast) is
    /// the deliberate trade.
    fn message_reminder(&self, payload: &MessageReminderPayload) -> Result<(), UnkaiError>;

    /// The total unread count changed.  On desktop this repaints the
    /// tray badge and the Windows taskbar overlay *and* pushes the
    /// count to the frontend.
    fn unread_total_changed(&self, total: u32);

    /// The per-account unread split changed.  Kept separate from
    /// `unread_total_changed` so a failed per-account query still
    /// leaves the global count updated.
    fn unread_by_account_changed(&self, by_account: &HashMap<String, u32>);

    /// The custom-theme catalogue on disk changed.
    fn custom_themes_changed(&self);

    /// The profile registry changed (#534): a profile was created,
    /// renamed, re-iconed, or deleted, or the startup mode moved.
    /// The registry is machine-global, so unlike the other push
    /// channels this one is expected to reach EVERY window, not
    /// just the emitting profile's — a second profile's settings
    /// panel must repaint its profile list too (chunk 4, #535).
    fn profiles_changed(&self);

    /// Swap the running app's icon set (tray base bitmap, window /
    /// taskbar icon) to `style`.
    ///
    /// The only fallible method on the trait: it validates the style
    /// slug, and `set_logo_style` deliberately applies *before* it
    /// persists so a bad slug can't wedge the user on a style they
    /// can't undo.
    fn apply_logo_style(&self, style: &str) -> Result<(), UnkaiError>;
}

/// `calendars-updated` event payload (#236 follow-up).  Fired when
/// the cache flips a calendar's `read_only` flag — currently the
/// only writer is the CalDAV-write fallback below, but the event
/// is generic so other future flips (e.g. a successful re-sync
/// that rolls a calendar back to writable) can ride the same
/// channel.  The frontend listens, refetches `get_cached_calendars`,
/// and refreshes any `EventEditor` already mounted.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarsUpdatedPayload {
    pub nextcloud_account_id: Option<String>,
}

/// `outbox-updated` event payload (#276).  Fires whenever the
/// queue changes shape (enqueue / drain success / drain failure /
/// manual delete) so the frontend can re-read counts and refresh
/// the synthetic Outbox folder.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxUpdatedPayload {
    /// Total queued rows across every account.  Retained so anything
    /// reading the unscoped count (tray indicators, future global
    /// badges) keeps working without a follow-up call.
    pub total: u32,
    /// Per-account count map (#290).  Drives the sidebar's "render
    /// the synthetic Outbox folder?" decision per account so the
    /// folder no longer leaks into accounts that have nothing
    /// queued.  Accounts with zero queued rows are omitted.
    pub by_account: std::collections::HashMap<String, u32>,
}

/// `new-mail` event payload (Issue #16).  Fired by the background
/// poll for every envelope above the previously-seen high-water mark.
///
/// The Rust side deliberately does **not** raise the OS notification
/// itself.  It pushes this payload and the frontend decides whether
/// (and how) to display it.  Rationale: one permission check path
/// (in JS), one formatting path, and no risk of a background tick
/// racing the OS permission prompt.
#[derive(Debug, Clone, Serialize)]
pub struct NewMailPayload {
    pub account_id: String,
    pub folder: String,
    pub uid: u32,
    pub from: String,
    pub subject: String,
}

/// `mail-flags-updated` event payload (#255 follow-up).  Tells the
/// frontend "the cached envelopes for this (account, folder) had a
/// flag-only change — please re-read the cache".  Two emit sites:
///
///   * Compose's send path, right after stamping `replied_kind` /
///     flipping `\Answered` on the message we just answered, so the
///     reply icon appears in the mail list immediately rather than
///     waiting for the next user-initiated refresh.
///   * The poll path's catch-up flag refresh, when it detects a
///     `\Seen` / `\Flagged` / `\Answered` change made on another
///     mail client.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailFlagsUpdatedPayload {
    pub account_id: String,
    pub folder: String,
}

/// Payload pushed to the frontend on every fired reminder.
/// Mirrors the camelCase shape JS expects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventReminderPayload {
    /// Cached event id (`{nc_id}::{cal_path}::{uid}` for masters
    /// / singletons; the `::occ::{epoch}` suffix is included for
    /// expanded recurrence occurrences).  Frontend uses this to
    /// open the event in the editor when the user clicks "Show
    /// event" on the in-app reminder card.
    pub event_id: String,
    /// Bare VEVENT UID — used for the dismiss-state key so all
    /// occurrences of a recurring series share one dismiss
    /// entry.
    pub uid: String,
    pub summary: String,
    /// Event start in UTC RFC 3339 — the JS side localises for
    /// the toast body ("Meeting in 15 min" / "starts at 14:00").
    pub start: chrono::DateTime<chrono::Utc>,
    /// Event end in UTC RFC 3339.  Surfaced on the in-app card
    /// so the user can see the duration at a glance.
    pub end: chrono::DateTime<chrono::Utc>,
    /// Free-text location string from the VEVENT (may itself
    /// contain a meeting URL — Nextcloud Calendar puts the Talk
    /// URL here).  `None` when the event has no LOCATION.
    pub location: Option<String>,
    /// Attendee email list — the in-app card surfaces the first
    /// few + a "+N more" tail.
    pub attendees: Vec<String>,
    /// First HTTP(S) URL found in URL / LOCATION / DESCRIPTION,
    /// or `None` when the event isn't a meeting at all.  Drives
    /// the per-event gate (`meeting_reminders_enabled` vs
    /// `calendar_reminders_enabled`) and the "Join meeting"
    /// affordance on the in-app card.
    pub meeting_url: Option<String>,
    /// Lead time the reminder fired at, in minutes.  Lets the
    /// JS side word the toast appropriately ("Now" / "in 5 min"
    /// / "in 1 hour").
    pub minutes_before: i32,
}

/// `message-reminder` event payload.  camelCase like the other
/// reminder payloads; carries the same identity triple the
/// notification deep-link uses plus enough envelope data for the
/// frontend to word the toast without a cache round-trip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageReminderPayload {
    pub account_id: String,
    pub folder: String,
    pub uid: u32,
    pub from: String,
    pub subject: String,
}

//! Core domain models shared across all Nimbus crates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// App-wide user preferences (not per-account).
///
/// Persisted to a single JSON file (`app_settings.json`) alongside
/// `accounts.json`. The struct carries `#[serde(default)]` at the
/// top level so adding a new field in a future version silently
/// slots in its default value instead of failing to parse an
/// existing user's settings file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Close button hides the window instead of quitting the app.
    /// Users quit explicitly via the tray menu.
    pub minimize_to_tray: bool,
    /// Whether the background sync loop polls INBOX across all accounts.
    pub background_sync_enabled: bool,
    /// How often (seconds) to poll. Clamped to a 30s floor at runtime
    /// so a misconfigured file can't DOS the user's mail server.
    pub background_sync_interval_secs: u64,
    /// Whether to show OS-native toasts when new mail arrives.
    pub notifications_enabled: bool,
    /// Launch hidden to tray on app start.
    pub start_minimized: bool,
    /// Skeleton UI theme name (e.g. `"cerberus"`, `"modern"`,
    /// `"pine"`). The frontend keeps the canonical list of themes
    /// it knows how to render; this value is the user's selection
    /// and is set on `<html data-theme="…">` at startup.
    pub theme_name: String,
    /// Whether the UI follows the OS light/dark preference, or
    /// is pinned to one. Applied via `<html data-mode="…">`.
    pub theme_mode: ThemeMode,
    /// Render HTML email bodies on a forced white canvas.
    ///
    /// HTML emails almost always set inline text colours assuming a
    /// light page background — the dark text becomes unreadable in
    /// dark mode if we let it render against the app's surface
    /// colour. With this on (default), the email body wrapper is
    /// painted white regardless of the app theme. Turn off to let
    /// the email render against the app's background — useful for
    /// dark-themed emails or when a sender provides a proper
    /// dark-mode design.
    pub mail_html_white_background: bool,
    /// Automatically load remote images embedded in HTML mail
    /// without showing the "Remote images are blocked" banner.
    ///
    /// Default `false` (privacy-first): remote images are blocked
    /// per-message until the user clicks "Show images" or
    /// "Always show from <sender>". Tracking pixels are the
    /// canonical reason for the default — every load tells the
    /// sender the user opened the message. Turning this on
    /// trades that signal for the convenience of every sender's
    /// art rendering on first view.
    #[serde(default)]
    pub auto_load_remote_images: bool,
    /// After delete / archive, automatically open the row directly
    /// below the removed message (or above, if it was the last one).
    /// Default `true` — matches the standard mail-client triage
    /// behaviour.  Turn off to fall back to the previous behaviour
    /// where the reading pane goes blank after every delete.
    pub auto_advance_after_remove: bool,
    /// Default calendar for events created in CalendarView and for
    /// inbound RSVPs that the user accepts.  Stored as the app-side
    /// calendar id (`{nc_id}::{path}`) so it's stable across syncs.
    /// `None` = no preference yet — the EventEditor's calendar
    /// dropdown defaults to whatever `calendars[0]` resolves to,
    /// and the RSVP card's calendar picker forces the user to make
    /// a one-time choice the first time they accept an invite.
    #[serde(default)]
    pub default_calendar_id: Option<String>,
    /// Whether to fire desktop notifications ahead of calendar
    /// events that carry **any** meeting URL — Nextcloud Talk,
    /// Zoom, Meet, Teams, Jitsi, etc. (issues #123 + #203).
    /// `extract_meeting_url` walks URL / LOCATION / DESCRIPTION
    /// for the first HTTP(S) URL it finds, so the toggle is not
    /// platform-specific.  Lead time is taken from the event's
    /// own `VALARM` triggers; this flag is the master opt-out.
    /// Stored under the new name; `talk_reminder_enabled` is the
    /// historical alias so existing user settings still load.
    #[serde(alias = "talk_reminder_enabled")]
    pub meeting_reminders_enabled: bool,
    /// Whether to fire desktop notifications ahead of calendar
    /// events *without* a meeting URL (issue #203).
    /// Independent from `meeting_reminders_enabled` so users who
    /// only want meeting nudges can keep that on while muting
    /// the generic stream, and vice-versa.  Lead time again comes
    /// from the event's own `VALARM` triggers.
    #[serde(default = "default_true")]
    pub calendar_reminders_enabled: bool,
    /// When true, a meeting reminder firing at "now" (≤1 min
    /// lead — typically the "At event start" preset) opens the
    /// meeting URL in the user's browser straight away instead
    /// of surfacing the popup window with Join / Show event /
    /// Snooze actions.  Off by default (#203 follow-up): the
    /// popup is the consistent default surface, and users who
    /// want the old auto-join shortcut explicitly opt in.
    #[serde(default)]
    pub auto_open_meetings: bool,
    /// Launch Nimbus automatically when the user logs in (#131
    /// follow-up).  Backed by `tauri-plugin-autostart`, which
    /// registers an XDG autostart entry on Linux, a LaunchAgent
    /// on macOS, and an `HKCU…\Run` value on Windows.  The
    /// settings UI keeps this in lockstep with the OS state via
    /// the plugin's `enable` / `disable` IPCs.
    pub autostart_enabled: bool,
    /// User-imported Skeleton theme CSS files (#132 tier 2).
    /// Populated by `import_custom_theme` (Tauri command) — copies
    /// the picked file under
    /// `<config>/nimbus-mail/themes/<id>.css` and tracks its
    /// metadata here.  Frontend's theme picker merges this list
    /// with the stock catalogue.
    #[serde(default)]
    pub custom_themes: Vec<CustomTheme>,
    /// User-selected app-icon style (taskbar / tray / window).  One
    /// of the slugs known to the runtime (`storm`, `dawn`, `mint`,
    /// `sky`, `twilight`, `monochrome-black`, `monochrome-white`).
    /// `set_logo_style` swaps the running tray and main-window icon
    /// to this style and persists it; on next boot the chosen icon
    /// is reapplied before the window is shown.  Note: the .exe icon
    /// shown in Windows Explorer / macOS Finder *before launch* is
    /// baked into the binary at build time and is not affected by
    /// this setting — only what the user sees while the app runs.
    #[serde(default = "default_logo_style")]
    pub logo_style: String,
    /// Manual UI-scale multiplier (#191).  Applied via CSS
    /// `zoom` on the document root, so it scales every visual
    /// element including icons / SVGs / pixel-pinned widths
    /// uniformly.  1.0 = "as designed".  Range enforced
    /// frontend-side: 0.7 .. 1.5 in 0.05 increments.
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
    /// When true, the frontend ignores `ui_scale` and computes
    /// a scale from the screen width on every launch (#191).
    /// User interactions that change `ui_scale` directly
    /// (manual slider in Settings, Ctrl+wheel) flip this to
    /// false so the auto-scale stops fighting the user's
    /// explicit choice.
    #[serde(default = "default_true")]
    pub ui_scale_auto: bool,
    /// Display-language locale (#190).  One of the locales
    /// declared in `ui/project.inlang/settings.json` (currently
    /// `"en"` and `"de"`).  Empty string means "follow the
    /// runtime preferred-language strategy" — paraglide picks
    /// `navigator.language` when not pinned.  Stored as a string
    /// rather than an enum so adding a new locale is purely an
    /// `messages/<locale>.json` change with no migration.
    #[serde(default)]
    pub ui_locale: String,
    /// When true, the frontend ignores `ui_locale` and lets
    /// paraglide auto-pick from `navigator.language` on each
    /// launch.  Flipping the manual locale picker off (or
    /// picking an explicit language) sets this to false.
    #[serde(default = "default_true")]
    pub ui_locale_auto: bool,
    /// Master toggle for the URLhaus link-safety check (#165).
    /// When on (default), every link in a rendered email is
    /// looked up against the local URLhaus snapshot and rendered
    /// with a green "Safe" or red "Unsafe" pill; clicks on
    /// unsafe links go through a confirm modal.  When off, the
    /// pills are suppressed, links open without interception,
    /// and the background refresh worker sleeps.  Default-on
    /// matches the project's privacy-first defaults — the
    /// URLhaus list is fetched once an hour over plain HTTPS
    /// with no per-user identifiers, so the privacy cost is
    /// negligible compared to the value of catching malware
    /// links before the user clicks them.
    #[serde(default = "default_true")]
    pub link_check_enabled: bool,
    /// Master toggle for the EventEditor's location autocomplete
    /// + inline map preview (#280).  Default **off** — the
    /// feature sends each typed query to Nominatim
    /// (`nominatim.openstreetmap.org`) and the map preview iframe
    /// loads tiles from `openstreetmap.org`, both third-party
    /// services outside the user's Nextcloud trust boundary.
    /// Off-by-default keeps the Location field as a plain text
    /// input that never leaves the device; flipping it on opts
    /// the user into the convenience of geocoded suggestions and
    /// the inline pin.  The cached `geocode_cache` rows are
    /// preserved when the toggle flips off — they're just not
    /// consulted — so a later opt-back-in is instant.
    #[serde(default)]
    pub location_geocoding_enabled: bool,
}

fn default_logo_style() -> String {
    "storm".to_string()
}

/// Default UI-scale multiplier (#191).  1.0 means "as designed".
fn default_ui_scale() -> f32 {
    1.0
}

/// Serde helper for fields whose post-deserialise default is `true`
/// (rather than `false`, which is what `bool::default()` returns).
/// Used by settings flags added after v0.1.0 so older config files
/// load the new flag with the intended on-by-default value.
fn default_true() -> bool {
    true
}

/// One user-imported Skeleton theme — the metadata the picker
/// needs (#132 tier 2).  The actual CSS lives at `path`; we keep
/// the slug, label, and description here so the picker can show
/// a row without parsing the file every time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomTheme {
    /// Theme slug — matches the value declared inside the CSS
    /// file's `[data-theme="…"]` selector.  This is what we set
    /// on `<html data-theme="…">` to activate the theme.
    pub id: String,
    /// Human-readable label shown in the picker.  Defaults to
    /// the imported file's stem on first import; the user can
    /// rename later.
    pub label: String,
    /// One-line description shown next to the label.
    #[serde(default)]
    pub description: String,
    /// Absolute path to the CSS file under
    /// `<config>/nimbus-mail/themes/`.  Stored absolute so the
    /// frontend can pass it through `convertFileSrc` without
    /// having to know our data-dir layout.
    pub path: String,
}

/// Light/dark mode selection. `System` follows the OS preference
/// (`prefers-color-scheme`) and reacts live when the user changes
/// their OS theme; `Light` / `Dark` pin the mode regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            minimize_to_tray: true,
            background_sync_enabled: true,
            // Tightened from 5 minutes to 60s as part of the icon-
            // rail shell pass: the manual "Refresh" button is gone
            // from the sidebar, so the background loop is the only
            // thing keeping the inbox fresh between the on-view-
            // switch poll and the user's next interaction. 60s is
            // the modern-client floor; users who care about server
            // load can bump it in Settings (30s hard floor still
            // enforced at runtime).
            background_sync_interval_secs: 60,
            notifications_enabled: true,
            start_minimized: false,
            theme_name: "cerberus".to_string(),
            theme_mode: ThemeMode::System,
            mail_html_white_background: true,
            auto_load_remote_images: false,
            auto_advance_after_remove: true,
            default_calendar_id: None,
            meeting_reminders_enabled: true,
            calendar_reminders_enabled: true,
            auto_open_meetings: false,
            autostart_enabled: false,
            custom_themes: Vec::new(),
            logo_style: default_logo_style(),
            ui_scale: default_ui_scale(),
            ui_scale_auto: true,
            ui_locale: String::new(),
            ui_locale_auto: true,
            link_check_enabled: true,
            // Off by default — the location autocomplete + map
            // preview send each typed query to Nominatim and load
            // tiles from openstreetmap.org, both outside the
            // user's Nextcloud trust boundary.  Users opt in
            // explicitly from General Settings (#280).
            location_geocoding_enabled: false,
        }
    }
}

/// Represents an email account configured by the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    /// Whether to prefer JMAP over IMAP when available.
    #[serde(default)]
    pub use_jmap: bool,
    /// Base URL of the JMAP server (e.g. `https://mail.example.com`).
    /// Only used when `use_jmap` is true. Discovered automatically
    /// during account setup if the server supports `.well-known/jmap`.
    #[serde(default)]
    pub jmap_url: Option<String>,
    /// Signature appended below new messages composed from this
    /// account.  Empty/None = no signature.  The frontend renders
    /// the standard RFC 3676 `-- ` separator before it.
    ///
    /// As of #248 this is rich HTML produced by the Tiptap editor in
    /// AccountSettings — bold / italic / lists / links / inline
    /// images all round-trip end-to-end.  Legacy plain-text
    /// signatures saved before #248 still work: Compose's
    /// `signatureBlock` detects whether the value contains HTML tags
    /// and either passes it through verbatim or runs the historical
    /// escape-and-`<br>` path.  No migration is needed; users who
    /// open the settings panel after the upgrade and save will end
    /// up with an HTML signature for free.
    #[serde(default)]
    pub signature: Option<String>,
    /// User-defined "folder name contains X → use icon Y" rules.
    /// The Sidebar applies these *before* its built-in icon
    /// heuristics so a user can theme their own folders ("Bank",
    /// "Amazon", a project name, …) without having to wait for the
    /// app to ship a hard-coded mapping. Per-account so users with
    /// different filing schemes on different mail accounts don't
    /// have to share one global list.
    #[serde(default)]
    pub folder_icons: Vec<FolderIconRule>,
    /// Per-folder icon overrides keyed by the full folder path. This
    /// is the "I right-clicked → Change icon" entry point — beats
    /// every other icon source (including special-use attributes)
    /// so if the user pins 📮 on their Inbox they actually get 📮,
    /// not whatever our default would be. Keyed by full path so
    /// `INBOX/Projects/2026` and `Projects/2026` can each carry
    /// their own choice without one matching the other.
    #[serde(default)]
    pub folder_icon_overrides: std::collections::HashMap<String, String>,
    /// TLS certificates the user has explicitly trusted for this
    /// account — typically self-signed certs on a personal mail
    /// server that webpki-roots wouldn't normally accept. Each
    /// entry is added to the rustls config's root store, so a
    /// matching cert chain validates as if it were CA-signed.
    /// Per-account so trust granted to one mail server can't
    /// silently apply to another.
    #[serde(default)]
    pub trusted_certs: Vec<TrustedCert>,
    /// Optional emoji chosen by the user to render in the
    /// IconRail's account avatar in place of the initials
    /// bubble (issue #115).  Free-form string so a future
    /// "use a contact photo" path can drop into the same slot
    /// without a schema change; the UI treats anything
    /// non-empty as the emoji and falls back to initials when
    /// `None`.
    #[serde(default)]
    pub emoji: Option<String>,
    /// Display order in the IconRail's account avatar list
    /// (issue #115).  Lower values render first; ties break on
    /// `id` so the order is stable across re-renders.  Defaults
    /// to `0` for back-compat — new accounts inherit `0` and
    /// new sort assignments run on top of that.
    #[serde(default)]
    pub sort_order: i32,
    /// Human's full name for the From: header (issue #115),
    /// e.g. `"Alex Morgan"`.  Separate from `display_name`
    /// (which is the account *label* — "Work", "Personal").
    /// `None` falls back to `display_name`, preserving the
    /// pre-115 behaviour for users who haven't set it.
    #[serde(default)]
    pub person_name: Option<String>,
}

/// One TLS leaf certificate the user has chosen to trust for an
/// account. We keep the raw DER bytes so the cert can be plugged
/// straight into `rustls::RootCertStore` (and into lettre's
/// `add_root_certificate`) on every connect, plus the SHA-256
/// fingerprint for display in settings ("you trust 4 certificates
/// for this account: aa:bb:cc:…") and a human-readable host /
/// added-on date so the user can audit the list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedCert {
    /// Raw DER-encoded certificate bytes. Stored as a `Vec<u8>` so
    /// the JSON representation is a base64 string under the hood
    /// (serde-bytes if we needed it; serde's default for `Vec<u8>`
    /// is an array of integers — works fine for a few-hundred-byte
    /// cert and stays human-debuggable).
    pub der: Vec<u8>,
    /// SHA-256 of the DER bytes, lowercase hex with `:` separators
    /// every two characters (`aa:bb:cc:…`). This is what the user
    /// compared against their server when they trusted it; we
    /// surface it in settings so they can confirm what's stored.
    pub sha256: String,
    /// Hostname this cert was trusted *for*. Just informational —
    /// rustls handles the actual hostname matching during the
    /// handshake, so a cert valid for `mail.example.com` won't
    /// silently extend trust to `other.example.com`.
    pub host: String,
    /// Unix epoch seconds when the cert was added. Lets the
    /// settings UI render "trusted on Jan 5" so a stale entry is
    /// recognisable.
    pub added_at: i64,
}

/// One "folder name contains keyword → show icon" rule. `keyword`
/// is matched case-insensitively against the folder's name (and the
/// last hierarchy segment, so `INBOX/Bank` and `Bank` both match
/// `bank`). `icon` is whatever the user typed — a single emoji is
/// the expected case but we don't enforce it; the sidebar just
/// drops the string into the icon slot verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderIconRule {
    pub keyword: String,
    pub icon: String,
}

/// Lightweight email metadata for list views.
///
/// This is what we fetch when populating the mail list sidebar — just
/// enough to render a row. Full body / HTML / attachments come from
/// a separate `fetch_message` call when the user clicks an email.
///
/// `uid` is the IMAP UID within the folder and uniquely identifies the
/// message across sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailEnvelope {
    pub uid: u32,
    pub folder: String,
    pub from: String,
    pub subject: String,
    pub date: DateTime<Utc>,
    pub is_read: bool,
    pub is_starred: bool,
    /// Mirrors the IMAP `\Answered` system flag (#255).  Refreshed
    /// on every envelope re-fetch.  Drives the generic reply icon
    /// in the mail list for messages the user answered before this
    /// feature shipped or answered from a different client; the
    /// per-kind `replied_kind` below takes precedence when set.
    /// `#[serde(default)]` so older cached payloads deserialise
    /// cleanly.
    #[serde(default)]
    pub is_answered: bool,
    /// Nimbus-only metadata recording *how* the user replied
    /// (#255): `"reply"`, `"reply-all"`, or `"meeting"`.  IMAP
    /// carries one boolean answered bit, but the user's intent
    /// (which icon to show) is something only we know — we
    /// stamp this on the original message after Compose's send
    /// path succeeds.  `None` means we didn't track a reply via
    /// Nimbus; the UI then falls back to `is_answered` for the
    /// icon decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replied_kind: Option<String>,
    /// Owning account id. Populated when envelopes are read out of the
    /// cache (where `account_id` is a column on every row) so the UI
    /// can render an account label in unified-inbox mode and route the
    /// "open message" click to the right account. IMAP/JMAP clients
    /// don't know their own account id, so they leave this empty —
    /// the cache write-through stamps it from the call site, and the
    /// cache read paths fill it back in. `#[serde(default)]` keeps
    /// older cached payloads parsing cleanly.
    #[serde(default)]
    pub account_id: String,
}

/// Represents an email message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Email {
    pub id: String,
    pub account_id: String,
    pub folder: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub date: DateTime<Utc>,
    pub is_read: bool,
    pub is_starred: bool,
    pub has_attachments: bool,
    /// Metadata for each attachment on the message — no bytes. Kept
    /// empty when `has_attachments` is false. The bytes are fetched on
    /// demand via a separate command so opening a message with a 50 MB
    /// attachment is still snappy.
    ///
    /// `#[serde(default)]` keeps older cached payloads (written before
    /// this field existed) deserialising cleanly — they come back as an
    /// empty list, which lines up with `has_attachments=false` for
    /// messages from before the attachment metadata landed.
    #[serde(default)]
    pub attachments: Vec<EmailAttachment>,
}

/// Metadata for one attachment on a received email.
///
/// The bytes are NOT carried here — they can be many megabytes and
/// would make every message fetch/cache hit that size. Instead we
/// expose enough to render an attachment chip and to later request the
/// bytes via `download_email_attachment` using `(folder, uid, part_id)`.
///
/// `part_id` is the index of this attachment among the message's
/// attachments (0, 1, 2, …) as `mail-parser` orders them. It's stable
/// for a given raw message — we re-parse on download and pick the same
/// index. Storing an opaque index rather than a MIME part path keeps
/// this JSON-friendly and avoids leaking parser internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailAttachment {
    /// Display filename (from `Content-Disposition: filename` or
    /// `Content-Type: name`). Defaults to `"attachment"` if the server
    /// sent neither — we'd rather show a label than hide the file.
    pub filename: String,
    /// MIME type, e.g. `"application/pdf"`. Defaults to
    /// `"application/octet-stream"` when missing.
    pub content_type: String,
    /// Decoded size in bytes. `None` if the parser couldn't determine
    /// it (rare — most attachments are base64/quoted-printable with a
    /// deterministic decoded length).
    pub size: Option<u64>,
    /// Zero-based index into the parsed message's attachment list.
    /// Used as a stable handle for re-fetching the bytes on demand.
    pub part_id: u32,
    /// RFC 2392 Content-ID, when the MIME part carried one. Lifted
    /// from the message's `Content-ID` header by `mail-parser` —
    /// without the surrounding angle brackets — so a body anchor
    /// `<a href="cid:abc-123">` can resolve to this attachment by
    /// `cid_str.eq_ignore_ascii_case(att.content_id.as_deref()?)`.
    /// `None` when the attachment isn't referenced inline.
    /// `#[serde(default)]` keeps cached payloads from before this
    /// field existed deserialising cleanly as `None`.
    #[serde(default)]
    pub content_id: Option<String>,
}

/// Represents an IMAP mailbox folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    /// Full folder name (e.g. "INBOX", "INBOX/Work")
    pub name: String,
    /// Hierarchy delimiter used by the server (e.g. "/" or ".")
    pub delimiter: Option<String>,
    /// IMAP folder attributes (e.g. \Sent, \Trash, \Drafts)
    pub attributes: Vec<String>,
    /// Number of unseen (unread) messages in this folder.
    /// `None` if the server didn't respond to the STATUS query.
    pub unread_count: Option<u32>,
}

/// Represents an email to be composed and sent via SMTP.
///
/// Unlike `Email` (which models a received message), this struct
/// carries only the fields needed for *sending*: recipients, subject,
/// body (plain text and/or HTML), and optional attachments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingEmail {
    /// Sender address (e.g. "alice@example.com")
    pub from: String,
    /// Primary recipients
    pub to: Vec<String>,
    /// Carbon-copy recipients
    pub cc: Vec<String>,
    /// Blind carbon-copy recipients
    pub bcc: Vec<String>,
    /// Optional Reply-To address (if different from `from`)
    pub reply_to: Option<String>,
    /// Subject line
    pub subject: String,
    /// Plain-text body (at least one of body_text / body_html should be set)
    pub body_text: Option<String>,
    /// HTML body
    pub body_html: Option<String>,
    /// File attachments
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// iTIP calendar part (#58).  When present, the SMTP layer
    /// emits the canonical iMIP MIME structure: a
    /// `text/calendar; method=…` alternative inside
    /// `multipart/alternative` (so RFC-compliant mail clients
    /// recognise the message as an invite and surface native
    /// Accept/Decline/Tentative buttons), plus a downloadable
    /// `.ics` attachment for clients that prefer to import via
    /// the file.  `None` for ordinary mails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar_part: Option<CalendarPart>,
    /// Skip the IMAP APPEND-to-Sent step that normally fires after
    /// SMTP delivery (#58).  Set on auto-generated mails the user
    /// didn't actively compose — calendar-grid invite mails and
    /// RSVP REPLYs — so the Sent folder doesn't fill up with
    /// machinery the user never wrote.  Compose-driven sends keep
    /// the default `false` because the user expects to see the
    /// mail they typed land in Sent.
    #[serde(default)]
    pub skip_sent_copy: bool,
}

/// Calendar payload emitted as the iMIP `text/calendar` body
/// alternative.  `method` is the iTIP method (REQUEST / REPLY /
/// CANCEL etc.); `ics` is the full VCALENDAR/VEVENT body
/// `nimbus_caldav::ical::build_ics_with_method` produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarPart {
    /// iTIP method (REQUEST, REPLY, CANCEL, …).
    pub method: String,
    /// Full ICS body — what `text/calendar` parts carry verbatim.
    pub ics: String,
}

/// A file attachment for an outgoing email.
///
/// The raw bytes are held in memory. For large files, consider
/// streaming from disk in the future.
///
/// `data` is serialised as a JSON array of bytes — the Svelte frontend
/// reads the picked file with `FileReader.readAsArrayBuffer` and sends
/// `Array.from(new Uint8Array(buffer))`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Display filename (e.g. "report.pdf")
    pub filename: String,
    /// MIME type (e.g. "application/pdf")
    pub content_type: String,
    /// Raw file contents
    pub data: Vec<u8>,
    /// RFC 2392 Content-ID, used when the body HTML contains a
    /// `<a href="cid:…">` reference to this attachment (the `/`
    /// attachment-picker shortcut in Compose). Optional because
    /// legacy attachment payloads predate the field — we treat an
    /// absent `content_id` the same as "no inline reference".
    #[serde(default)]
    pub content_id: Option<String>,
}

/// A persistent Nextcloud connection.
///
/// One `NextcloudAccount` can be shared across multiple mail accounts —
/// users often have several email identities but a single Nextcloud
/// instance that backs attachments, Talk rooms, contacts and calendars.
/// That's why this lives as its own top-level record (separate from
/// `Account`) and is not keyed by email.
///
/// The `app_password` itself is **never** stored here — it lives in the
/// OS keychain under service `nimbus-mail-nextcloud` keyed by `id`.
/// `capabilities` is cached at connect time so the UI can show which
/// Nextcloud apps are available without a round-trip on every render.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextcloudAccount {
    /// Stable UUID; used as the keychain account key for the app password.
    pub id: String,
    /// Base URL of the Nextcloud server, e.g. `https://cloud.example.com`.
    /// Stored without trailing slash.
    pub server_url: String,
    /// Nextcloud login name returned by Login Flow v2. Often differs from
    /// the user's email — it's whatever NC uses to identify the user.
    pub username: String,
    /// Optional pretty name shown in the UI — pulled from
    /// `/ocs/v2.php/cloud/user` after login when available.
    pub display_name: Option<String>,
    /// What the server supports, snapshotted at connect time.
    pub capabilities: Option<NextcloudCapabilities>,
    /// User-trusted self-signed cert fingerprints (#253).  Same
    /// shape as `Account::trusted_certs` for IMAP/SMTP — every
    /// reqwest client built for this account (Nextcloud OCS,
    /// CalDAV, CardDAV, Notes API) plugs these into the rustls
    /// verifier so traffic to a self-hosted server with a
    /// non-public CA actually goes through.  Empty for the common
    /// Let's-Encrypt / public-CA case; populated by the user
    /// trusting a cert via the setup-time probe prompt.
    /// `#[serde(default)]` so existing accounts.json files
    /// without this field deserialise cleanly.
    #[serde(default)]
    pub trusted_certs: Vec<TrustedCert>,
}

/// Boolean flags for which Nextcloud apps the connected server offers.
///
/// Nextcloud's capabilities endpoint returns a deep, provider-specific
/// JSON tree; we reduce it to the handful of bits the UI actually
/// branches on. Refetched (via `fetch_capabilities`) when the user
/// explicitly asks to refresh the connection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NextcloudCapabilities {
    /// Nextcloud server version (e.g. "28.0.4"). Useful for feature gates.
    pub version: Option<String>,
    /// Nextcloud Talk (spreed) is installed and enabled.
    pub talk: bool,
    /// Files app — attachments, file sharing. Effectively always true on
    /// a working NC install, but we still check to be defensive.
    pub files: bool,
    /// CalDAV calendar endpoint is available.
    pub caldav: bool,
    /// CardDAV contact endpoint is available.
    pub carddav: bool,
    /// Nextcloud Office / Collabora (the `richdocuments` app id) is
    /// installed and enabled. When true, the attachment-click flow
    /// can open `.docx` / `.odt` / `.xlsx` etc. in an embedded
    /// editor; when false the UI falls back to plain download.
    /// `#[serde(default)]` so capability snapshots cached before
    /// this field existed deserialise as `false`.
    #[serde(default)]
    pub office: bool,
    /// Nextcloud Notes (`notes` app id) is installed and enabled.
    /// Drives the "Notes" chip in NextcloudSettings so the user
    /// can tell at a glance whether the in-app Notes view will
    /// have anything to show.
    #[serde(default)]
    pub notes: bool,
    /// Nextcloud Tasks (`tasks` app id) is installed and enabled.
    /// Same purpose as `notes` — chip-only signal in settings.
    #[serde(default)]
    pub tasks: bool,
}

/// Represents a contact from CardDAV / Nextcloud.
///
/// `id` is a stable app-side UUID we generate the first time we see a
/// vCard — handy as a single string the UI can use as a key. The
/// CardDAV side is identified by the triple
/// `(nextcloud_account_id, addressbook, vcard_uid)`; that triple lives
/// only in the cache, the UI never deals with it.
///
/// `photo_data` is the decoded image bytes (vCard PHOTO is base64 in
/// the wire format, we decode once on import). Kept on the contact row
/// so the autocomplete dropdown can render thumbnails without a
/// separate fetch — the standard mail-client behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub id: String,
    /// Which Nextcloud account this contact came from. Lets the UI
    /// group contacts by source if a user has more than one NC server.
    pub nextcloud_account_id: String,
    /// CardDAV addressbook collection path the contact lives in
    /// (e.g. `https://cloud/.../calendars/user/contacts/`).  The
    /// contacts UI's addressbook filter compares against this.
    /// Empty for legacy rows written before the field was added.
    #[serde(default)]
    pub addressbook: String,
    pub display_name: String,
    /// Email addresses paired with a kind hint (vCard `EMAIL;TYPE=…`).
    /// Same shape pattern as `phone` and `addresses` so the UI can
    /// group "home / work / other" the way Nextcloud Contacts does.
    pub email: Vec<ContactEmail>,
    /// Phone numbers paired with a kind hint (vCard `TEL;TYPE=…`).
    /// Same shape pattern as `addresses` so the UI can group "home /
    /// work / mobile / fax / other" the way Nextcloud Contacts does.
    pub phone: Vec<ContactPhone>,
    pub organization: Option<String>,
    /// MIME type of `photo_data` (e.g. "image/jpeg"); `None` if no photo.
    pub photo_mime: Option<String>,
    /// Raw decoded photo bytes. Serialised as a JSON byte array so the
    /// frontend can wrap it in a `Blob` URL for `<img src>`.
    pub photo_data: Option<Vec<u8>>,
    /// Job title (vCard `TITLE`) — separate from `organization`'s
    /// company name. Often paired with org in the contact card UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Postal addresses (vCard `ADR`). Multiple allowed; each carries
    /// a kind hint (`home` / `work` / `other`) so the UI can group
    /// them like Nextcloud Contacts does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<ContactAddress>,
    /// Birthday (vCard `BDAY`). Stored as the raw vCard text — date
    /// formats vary (`19851031`, `1985-10-31`, `--10-31` for missing
    /// year) and parsing here would lose information the UI can still
    /// render verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub birthday: Option<String>,
    /// Personal/work URLs (vCard `URL`). Multiple allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    /// Free-form note (vCard `NOTE`). Single multi-line string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// vCard `KIND` (RFC 6350 §6.1.4) — `"group"` for a mailing-
    /// list / group card, empty for an individual.  The IPC
    /// surface filters group cards out of the regular contact
    /// list and exposes them through dedicated group commands
    /// (#133 / #113), so this field is mostly informational on
    /// the wire.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// `CATEGORIES` tag list (RFC 6350 §6.7.1) — what NC's
    /// Contacts UI calls "Kontaktgruppen" and what iOS shows
    /// as Groups.  Free-form strings; the contacts UI treats
    /// distinct values aggregated across all cached cards as
    /// the live "groups" list (#133 redesign).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,

    // ── #143: vCard 4 fields surfaced in the contact form ──────
    //
    // Every new field below carries `#[serde(default)]` so a
    // contact row written before the field existed continues to
    // deserialise — the missing slot lands on its type's zero
    // value (None / "" / Vec::new()).  The vCard
    // parser/serialiser in `nimbus-carddav` rounds these through
    // their RFC 6350 properties; UI surfaces them as part of
    // the redesigned form.
    /// `N` structured name (RFC 6350 §6.2.2) — the breakdown of
    /// the formatted name into family / given / additional /
    /// prefixes / suffixes pieces.  When the user fills these
    /// in, `display_name` (the vCard FN) is auto-derived from
    /// "{prefixes} {given} {additional} {family} {suffixes}" at
    /// save time.  Optional: a contact created with only FN
    /// keeps an empty StructuredName and the form falls back to
    /// editing FN directly.
    #[serde(default, skip_serializing_if = "StructuredName::is_empty")]
    pub structured_name: StructuredName,
    /// `NICKNAME` (RFC 6350 §6.2.3) — friendly handle.  Single
    /// value; the vCard property allows comma-separated lists,
    /// but in practice every client treats it as one nickname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// `ANNIVERSARY` (RFC 6350 §6.2.6) — same wire format as
    /// `BDAY`, kept as raw text for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anniversary: Option<String>,
    /// `GENDER` (RFC 6350 §6.2.7).  Standardly one of `M`/`F`/`O`/
    /// `N`/`U` plus an optional free-form identity component
    /// after a `;` separator.  We keep the raw vCard string so
    /// the user can write "non-binary" or whatever they want
    /// without us imposing an enum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    /// `IMPP` (RFC 6350 §6.4.3) — instant-messaging URIs (Matrix,
    /// XMPP, Telegram, Signal, …).  Each entry carries a kind
    /// hint (matrix / xmpp / telegram / signal / other) so the
    /// UI can group them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impp: Vec<ContactImpp>,
    /// `ROLE` (RFC 6350 §6.6.2) — function the contact performs
    /// inside the org, distinct from `TITLE` which is the job
    /// title.  E.g. ROLE="Project Lead", TITLE="Senior Engineer".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// `LANG` (RFC 6350 §6.4.4) — preferred languages, in
    /// preference order, as RFC 5646 BCP-47 tags (`en-US`,
    /// `de`, …).  Multiple entries allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// `GEO` (RFC 6350 §6.5.2) — living-location coordinates as
    /// the vCard wire form `geo:<lat>,<lon>`.  We keep it as
    /// the raw URI so the UI can render a map link directly
    /// without re-encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geo: Option<String>,
    /// `TZ` (RFC 6350 §6.5.1) — timezone, either an IANA tag
    /// (`Europe/Berlin`) or a UTC offset (`+02:00`).  Free-form
    /// string; the form uses an autocomplete against the IANA
    /// list but accepts any value the user types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// `KEY` (RFC 6350 §6.8.1) — public-key material (PGP, X.509)
    /// either inline (`data:application/pgp-keys;base64,...`) or
    /// referenced by URL.  Round-tripped through the parser /
    /// serialiser today; the form UI is deliberately deferred
    /// to a later issue dedicated to key management.  Multiple
    /// keys allowed (one per format).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

/// Structured name parts (vCard `N`, RFC 6350 §6.2.2).  A non-
/// empty StructuredName takes priority over `display_name` for
/// the FN derivation at save time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StructuredName {
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub given: String,
    #[serde(default)]
    pub additional: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub suffix: String,
}

impl StructuredName {
    /// `true` when no field has any non-whitespace content.
    /// Drives the `skip_serializing_if` filter on the contact
    /// row so an empty N doesn't pollute the JSON wire payload.
    pub fn is_empty(&self) -> bool {
        self.family.trim().is_empty()
            && self.given.trim().is_empty()
            && self.additional.trim().is_empty()
            && self.prefix.trim().is_empty()
            && self.suffix.trim().is_empty()
    }
}

/// One IMPP (instant-messaging) entry.  `kind` is a tag the UI
/// uses to group rows ("matrix" / "xmpp" / "telegram" / "signal"
/// / "other"); `value` is the platform-native URI
/// (`matrix:@user:server` / `xmpp:user@server` / `tg://...`
/// / `https://signal.me/#p/...`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContactImpp {
    pub kind: String,
    pub value: String,
}

/// One contact group / mailing list (vCard `KIND:group`,
/// RFC 6350 §6.1.4 + §6.6.5).  Stored as a regular vCard in the
/// owning address book — no separate CardDAV endpoint or
/// server-side mail expansion involved.  Members are referenced
/// by UID URI (`urn:uuid:<uid>`) the same way Apple Contacts and
/// Nextcloud Contacts do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactGroup {
    /// Composite app-side id — same shape as `Contact.id`
    /// (`{nc_account}::{addressbook_path}::{uid}`) so frontend
    /// callers can route through the existing per-account
    /// resolvers.
    pub id: String,
    pub nextcloud_account_id: String,
    /// Group display name (vCard `FN`).
    pub display_name: String,
    /// Bare VEVENT-style UIDs of the contacts in this group.
    /// Resolved against the contacts cache lazily — a group
    /// surviving the deletion of one of its members is normal
    /// (the membership row just dangles until the user prunes).
    #[serde(default)]
    pub member_uids: Vec<String>,
    /// Optional emoji shown in place of the initials avatar in
    /// the contacts sidebar.  Populated locally; never written
    /// back to the vCard since RFC 6350 has no equivalent slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// Local-only flag — `true` removes the group from the
    /// contacts sidebar AND from address-autocomplete results.
    /// Mirrors the calendar `hidden` toggle so users with many
    /// imported NC groups can declutter without deleting.
    #[serde(default)]
    pub hidden: bool,
}

/// One postal address from a vCard `ADR` property.
///
/// vCard 4 splits the address into seven fields (PO box, extended,
/// street, locality, region, postal code, country). We omit the
/// PO-box and extended slots — Nextcloud Contacts doesn't surface
/// them either — and keep the rest as plain strings the UI renders
/// in standard "street, city region postal, country" order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactAddress {
    /// "home", "work", or "other". Lower-cased, and `"other"` is the
    /// fallback when the vCard `TYPE` parameter is absent or
    /// unrecognised.
    pub kind: String,
    pub street: String,
    pub locality: String,
    pub region: String,
    pub postal_code: String,
    pub country: String,
}

/// One phone number from a vCard `TEL` property paired with a kind
/// hint pulled from its `TYPE=` parameter. vCard 4 lets `TYPE` carry
/// a comma-separated list — we pick the first recognised value
/// (`home` / `work` / `cell` / `fax`) and fall back to `"other"` so
/// no entry ever loses its value just because we couldn't classify
/// it. Mirrors the `ContactAddress` pattern so the UI grouping
/// works the same way Nextcloud Contacts does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactPhone {
    pub kind: String,
    pub value: String,
}

/// One email address from a vCard `EMAIL` property paired with a
/// kind hint pulled from its `TYPE=` parameter. Recognises `home`
/// and `work`; `INTERNET` (a vCard-3 legacy meaning "this is an
/// email address") is treated as no information and falls back to
/// `"other"`. Same shape as `ContactPhone`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactEmail {
    pub kind: String,
    pub value: String,
}

/// Represents a calendar event from CalDAV / Nextcloud.
///
/// The recurrence fields below (`rrule`, `rdate`, `exdate`,
/// `recurrence_id`) are **captured during sync but not yet expanded**.
/// The struct always describes one concrete instance — the master for
/// non-recurring events, or the first occurrence of a recurring
/// series. See issue #47 for the expansion work that turns these
/// fields into visible additional occurrences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub summary: String,
    pub description: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub location: Option<String>,
    /// Raw RRULE value, e.g. `FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20270101T000000Z`.
    /// Stored as-is so the eventual expander doesn't re-parse from the
    /// iCalendar source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rrule: Option<String>,
    /// Extra occurrence dates added to the series (`RDATE`). Mostly
    /// empty in practice — many calendar UIs don't expose it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rdate: Vec<DateTime<Utc>>,
    /// Cancelled occurrences (`EXDATE`). Present on cancelled
    /// instances of an otherwise-recurring series.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exdate: Vec<DateTime<Utc>>,
    /// If this VEVENT is an override for a specific occurrence of a
    /// recurring series, this holds the original start time of that
    /// occurrence (the `RECURRENCE-ID`). `None` for masters and for
    /// non-recurring events. The shared UID between master and
    /// override is in `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence_id: Option<DateTime<Utc>>,
    /// `URL` property — a link associated with the event (meeting URL,
    /// agenda doc, etc.). Free-form, the editor doesn't validate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `TRANSP` — `OPAQUE` (default — busy time) or `TRANSPARENT`
    /// (free time). The editor surfaces this as a "show as
    /// busy / free" picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transparency: Option<String>,
    /// `ATTENDEE` properties. Empty for events with no participants.
    /// We store name + email + the participant status the server last
    /// reported; the UI only edits the email list today.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<EventAttendee>,
    /// `VALARM` blocks. The editor exposes a single "remind me X
    /// before" picker, but the model carries a list so existing
    /// events with several alarms round-trip without losing data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reminders: Vec<EventReminder>,
    /// `GEO` property latitude (RFC 5545 §3.8.1.6) — stamped by
    /// the EventEditor's location-autocomplete pick (#280) so the
    /// inline map preview can drop a pin on the canonical place.
    /// `None` for events whose `LOCATION` is free-text without a
    /// geocoded match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    /// `GEO` property longitude — pairs with `latitude`.  Stored
    /// independently rather than as a tuple so the JSON shape
    /// surfaces both fields by name (the UI's IPC shape uses
    /// camelCase getters per field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
}

/// A single ATTENDEE property on a VEVENT.
///
/// Only the most-used fields are surfaced — CN (display name), the
/// `mailto:` email, and PARTSTAT (acceptance status). The full set
/// (ROLE, RSVP, CUTYPE, …) is preserved opaquely in `ics_raw` until
/// a follow-up issue surfaces them in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventAttendee {
    /// The email after `mailto:`. Required by the iCalendar spec.
    pub email: String,
    /// `CN=` parameter (e.g. `"Jane Doe"`). Optional — many invites
    /// only carry the email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub common_name: Option<String>,
    /// `PARTSTAT=` parameter — `NEEDS-ACTION` / `ACCEPTED` /
    /// `DECLINED` / `TENTATIVE`. `None` falls back to NEEDS-ACTION
    /// when written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// `ROLE=` parameter — `REQ-PARTICIPANT` (required) /
    /// `OPT-PARTICIPANT` (optional) / `CHAIR` / `NON-PARTICIPANT`.
    /// `None` is treated as `REQ-PARTICIPANT` per RFC 5545 §3.2.18.
    /// The EventEditor exposes Required / Optional / Chair as
    /// separate input fields and writes the appropriate ROLE here;
    /// the CalDAV PUT carries it through verbatim so Nextcloud's
    /// Calendar UI shows the role chip on each attendee row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Transient flag (RFC 6638 §7.3): emit
    /// `SCHEDULE-FORCE-SEND=REPLY` on this ATTENDEE's
    /// iCalendar property.  Set by `respond_to_invite` on the
    /// responding attendee's row so Sabre's CalDAV-Schedule
    /// plugin dispatches the REPLY iMIP unconditionally —
    /// without this, the broker's "is this change significant
    /// enough?" heuristics can decide to skip the send,
    /// leaving the organiser without a notification even
    /// though Sabre processed the PARTSTAT update.  Skipped
    /// from serde so it doesn't survive cache round-trips
    /// (it's purely an in-memory hint for the next PUT).
    #[serde(skip)]
    pub force_send_reply: bool,
}

/// A single VALARM block.
///
/// We model the most common reminder shape — a relative offset before
/// the event start — directly. The trigger is stored as **minutes
/// before** the event (positive = before, negative = after).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventReminder {
    /// Minutes before the event start. `15` means "fire 15 minutes
    /// before". Negative values fire after the start.
    pub trigger_minutes_before: i32,
    /// `ACTION` — `DISPLAY` (popup) or `EMAIL`. Defaults to `DISPLAY`
    /// when written if `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// One Nextcloud Notes document (#138).  Mirrors what the Notes
/// REST app exposes — a flat document keyed by an integer id with
/// an etag for optimistic-concurrency edits.  Lives in
/// `nimbus-core` rather than inside `nimbus-nextcloud` so the
/// store + Tauri layers can refer to a single canonical type;
/// the Nextcloud-specific wire shape is converted at the network
/// boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    /// Per-account note id (server-assigned by NC on create).
    pub id: u64,
    /// Owning Nextcloud account — needed because the cache mixes
    /// notes from multiple accounts in one table and the integer
    /// `id` only collides within an account.
    pub nextcloud_account_id: String,
    /// Optimistic-concurrency token from the server.  Sent back
    /// in `If-Match` on update; a 412 response means the body
    /// changed underneath us and the user has to merge.
    pub etag: String,
    /// `modified` Unix timestamp (seconds).  Sort key for the
    /// "recent" view and an input to the sync delta logic.
    pub modified: i64,
    /// First markdown line is conventionally treated as the
    /// title in NC; we store whatever the server returned so the
    /// UI doesn't have to re-derive it.
    pub title: String,
    /// `/`-separated category path.  Empty = uncategorized.  NC's
    /// web UI renders nested categories as folders, and so do
    /// we — `Joplin/ProjectX` becomes a `ProjectX` sub-folder
    /// under `Joplin`.
    pub category: String,
    /// Raw markdown body.  We do not pre-render to HTML; the
    /// editor + preview are responsible for that.
    pub content: String,
    /// User-pinned ⭐ flag.  Drives the "Favorites" virtual
    /// folder in the sidebar.
    pub favorite: bool,
}

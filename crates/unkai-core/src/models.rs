//! Core domain models shared across all Unkai crates.

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
    /// Group same-conversation messages under a single MailList row
    /// (#334).  Default on — the conversation badge collapses
    /// reply chains into one entry with an expand chevron.  Off
    /// renders every envelope as its own flat row (the pre-#277
    /// behaviour) for users who prefer chronological scrolling
    /// over conversation bundling.  The cache still computes
    /// `thread_id` either way, so toggling back on is a free
    /// re-render with no IMAP traffic.
    #[serde(default = "default_true")]
    pub conversation_view_enabled: bool,
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
    /// Launch Unkai automatically when the user logs in (#131
    /// follow-up).  Backed by `tauri-plugin-autostart`, which
    /// registers an XDG autostart entry on Linux, a LaunchAgent
    /// on macOS, and an `HKCU…\Run` value on Windows.  The
    /// settings UI keeps this in lockstep with the OS state via
    /// the plugin's `enable` / `disable` IPCs.
    pub autostart_enabled: bool,
    /// User-imported Skeleton theme CSS files (#132 tier 2).
    /// Populated by `import_custom_theme` (Tauri command) — copies
    /// the picked file under
    /// `<config>/unkai-mail/themes/<id>.css` and tracks its
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
    /// and inline map preview (#280).  Default **off** — the
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
    /// Override base URL for forward-geocoding (#259 follow-up).
    /// Empty string means "use the built-in default
    /// `https://nominatim.openstreetmap.org`".  Self-hosters
    /// can point this at their own Nominatim instance —
    /// Nominatim's posted usage policy actively recommends a
    /// private deployment for any volume above casual use.
    /// We trim trailing slashes at request time and append
    /// `/search` ourselves; the URL the user enters should be
    /// the base (e.g. `https://nominatim.example.com`).
    #[serde(default)]
    pub nominatim_base_url: String,
    /// What happens when the user clicks a `mail://acc/folder/uid`
    /// reference embedded in a note (#260).  Default `false` opens
    /// the message in a standalone reader window so the user keeps
    /// their place in the Notes view; flipping it on routes the
    /// click through the main app's view-switch instead (lands in
    /// the inbox at that message).
    #[serde(default)]
    pub notes_mail_open_in_view: bool,
    /// How to react when an incoming message carries a
    /// `Disposition-Notification-To:` read-receipt request
    /// (RFC 8098, #416).  `Ask` (default) surfaces a banner in the
    /// reading pane offering to send or decline the receipt;
    /// `Always` sends one automatically the first time the message
    /// is displayed; `Never` suppresses the banner entirely and
    /// sends nothing.  Read receipts leak reading behaviour to the
    /// sender, so nothing is ever sent without this setting or an
    /// explicit per-message click authorising it.
    #[serde(default)]
    pub mdn_response_mode: MdnResponseMode,
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
    /// `<config>/unkai-mail/themes/`.  Stored absolute so the
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

/// Response policy for incoming read-receipt requests (RFC 8098,
/// #416).  Lowercase on the wire (`"never"` / `"ask"` / `"always"`)
/// to match the JSON-over-IPC convention `ThemeMode` set.  `Ask` is
/// the default: receipts disclose reading behaviour, so the user
/// stays in the loop per message unless they explicitly opt into
/// `Always` (or shut the whole thing off with `Never`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MdnResponseMode {
    Never,
    #[default]
    Ask,
    Always,
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
            conversation_view_enabled: true,
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
            // Empty = fall back to the public Nominatim
            // endpoint at request time.  Setting this to a
            // self-hosted URL routes every typed query through
            // the user's own Nominatim instead.
            nominatim_base_url: String::new(),
            // Default off — `mail://` clicks in a note open a
            // standalone reader so the user keeps their place in
            // the editor.  Flipping this on routes the click
            // through the main view-switch instead.
            notes_mail_open_in_view: false,
            // Ask per message (#416) — read receipts disclose
            // reading behaviour, so nothing is sent without the
            // user's say-so.
            mdn_response_mode: MdnResponseMode::Ask,
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
    /// Hex fingerprint of the OpenPGP private key stored for this
    /// account (#57).  Display-only hint that a key exists; the
    /// armored key material itself lives in the OS keychain under
    /// the service `unkai-mail-pgp-private-key`, keyed by
    /// `account_id`, with the passphrase under
    /// `unkai-mail-pgp-passphrase` (set in Phase 4).  Surfaced in
    /// the AccountSettings "Encryption Keys" panel as
    /// "Key 9F2A…AAAA" so the user can confirm the right key is
    /// active without having to unlock the keychain.  `None`
    /// when the user hasn't imported a key for this account yet.
    #[serde(default)]
    pub pgp_key_fingerprint: Option<String>,
    /// SHA-256 fingerprint of the S/MIME (X.509) certificate stored for
    /// this account (#338) — colon-separated uppercase hex, the
    /// `openssl x509 -fingerprint -sha256` form.  Display-only hint that
    /// an identity exists; the `.p12` bundle (leaf cert + private key +
    /// chain) lives in the OS keychain under the service
    /// `unkai-mail-smime-private-cert`, keyed by `account_id`, with the
    /// passphrase under `unkai-mail-smime-passphrase`.  Surfaced in the
    /// S/MIME settings panel so the user can confirm the right
    /// certificate is active without unlocking the keychain.  `None`
    /// when the user hasn't imported a certificate for this account yet.
    #[serde(default)]
    pub smime_cert_fingerprint: Option<String>,
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
    /// Unkai-only metadata recording *how* the user replied
    /// (#255): `"reply"`, `"reply-all"`, or `"meeting"`.  IMAP
    /// carries one boolean answered bit, but the user's intent
    /// (which icon to show) is something only we know — we
    /// stamp this on the original message after Compose's send
    /// path succeeds.  `None` means we didn't track a reply via
    /// Unkai; the UI then falls back to `is_answered` for the
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
    /// RFC 5322 `Message-ID:` header — the canonical unique
    /// identifier for this mail across servers (#277).  Populated
    /// from the FETCH headers; cached to drive thread-grouping
    /// without re-parsing the body blob.  Round-trips through
    /// the cache as a separate column for fast index lookups.
    /// `None` for older cached envelopes that pre-date the v31
    /// schema migration; the IMAP fetch path back-fills on the
    /// next sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// RFC 5322 `In-Reply-To:` header — the immediate parent
    /// of this reply (#277).  Drives the inbox bundling
    /// (siblings sharing a parent collapse into one row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// RFC 5322 `References:` header parsed into the ordered
    /// chain of ancestor Message-IDs (oldest-first) (#277).
    /// Used to find the *root* of a thread (first entry) for
    /// grouping when `In-Reply-To` is missing or threads
    /// branched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references_ids: Vec<String>,
    /// Stable thread identity assigned by the local cache (#334):
    /// `references_ids[0]` for replies, the envelope's own
    /// `message_id` for chain roots, or a `solo:<account>:<folder>:<uid>`
    /// fallback for envelopes that have neither.  Two envelopes share
    /// `thread_id` iff they belong to the same conversation.  `None`
    /// for envelopes coming straight off the wire from IMAP/JMAP —
    /// the cache write-through path stamps it during upsert.  Also
    /// `None` for cached rows that pre-date the schema migration
    /// until the warm-up has run; the UI hides the count badge in
    /// that transient state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Number of cached members of this thread within
    /// `(account_id, folder)` (#334).  Maintained incrementally by
    /// the cache on every upsert / remove / move so the MailList
    /// row can paint the conversation badge from a single column
    /// read instead of grouping at query time.  `None` mirrors
    /// `thread_id` (envelope hasn't been assigned a thread yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_total_count: Option<u32>,
    /// Kebab-case `unkai_crypto::Protection` (#57) lifted from the
    /// `message_bodies` row via a LEFT JOIN when the envelope is
    /// read out of the cache.  Surfaces the encryption + signature
    /// state of the message in the mail-list row so it can render
    /// a shield-with-lock chip alongside the date — same data the
    /// MailView header chip uses.  `None` for messages whose body
    /// hasn't been fetched yet (the receive path stamps the
    /// column on first full open) and for plain-text mail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protection: Option<String>,
    /// Local-only "keep this at the top of the list" state (#414).
    /// No IMAP/JMAP equivalent exists, so the flag never leaves the
    /// cache: protocol clients always produce `false`, the cache
    /// read paths fill in the stored value, and envelope re-fetches
    /// can't clobber it (the upsert leaves the column alone).
    /// `#[serde(default)]` keeps older cached payloads parsing
    /// cleanly.
    #[serde(default)]
    pub is_pinned: bool,
    /// Sender-declared message priority parsed from the
    /// `X-Priority:` / `Importance:` headers at fetch time (#414):
    /// `"high"` or `"low"`.  `None` = normal priority (the
    /// overwhelmingly common case) — kept sparse so the cache
    /// column stays NULL for ordinary mail.  See
    /// [`priority_from_headers`] for the mapping rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// User-set priority for this message (#414): `"high"`,
    /// `"normal"`, or `"low"`.  Local-only — like `is_pinned`,
    /// there is no server-side equivalent, so it lives purely in
    /// the cache and wins over the header-derived `priority` when
    /// both are present (`"normal"` is a real value here precisely
    /// so the user can downgrade a sender's "high" back to
    /// nothing).  `None` = user never touched it; display falls
    /// back to `priority`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_override: Option<String>,
    /// Unix-epoch seconds at which the user asked to be reminded
    /// about this message (#415).  Local-only — like `is_pinned`,
    /// there is no server-side equivalent, so protocol clients
    /// always produce `None` and the cache read paths fill in the
    /// stored value.  The background scanner clears it back to
    /// `None` once the reminder has fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder_at: Option<i64>,
    /// Wire-only marker (#416): the envelope's top-level
    /// `Content-Type:` is `multipart/report;
    /// report-type=disposition-notification` — i.e. this message IS
    /// a read receipt some recipient sent back to us.  Stamped by
    /// the protocol clients from the same HEADER.FIELDS /
    /// header-property slice the protection check uses, and
    /// consumed by the sync path to fetch + parse the report body
    /// and update the matching sent-mail receipt record.  Never
    /// persisted: cache read paths always produce `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_mdn_report: bool,
}

/// Map the sender-declared priority headers to our two-value
/// priority scale (#414).
///
/// Checked in order — first header that yields a value wins:
///
/// 1. `X-Priority:` — the de-facto numeric convention: `1`/`2` mean
///    high, `4`/`5` mean low, `3` is normal.  Values often carry a
///    trailing label (`"1 (Highest)"`), so only the leading digit is
///    read.
/// 2. `Importance:` (RFC 2156) and its `X-MSMail-Priority:` twin —
///    the word forms `high` / `low` (`normal` maps to `None`).
///
/// Returns `"high"` / `"low"`, or `None` for normal / absent /
/// unparseable — so ordinary mail stays out of the cache column
/// entirely.
pub fn priority_from_headers(
    x_priority: Option<&str>,
    importance: Option<&str>,
    msmail_priority: Option<&str>,
) -> Option<String> {
    if let Some(v) = x_priority {
        match v.trim().chars().next() {
            Some('1') | Some('2') => return Some("high".into()),
            Some('4') | Some('5') => return Some("low".into()),
            _ => {}
        }
    }
    for v in [importance, msmail_priority].into_iter().flatten() {
        let v = v.trim();
        if v.eq_ignore_ascii_case("high") || v.eq_ignore_ascii_case("urgent") {
            return Some("high".into());
        }
        if v.eq_ignore_ascii_case("low") || v.eq_ignore_ascii_case("non-urgent") {
            return Some("low".into());
        }
    }
    None
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
    /// RFC 5322 `Message-ID:` (#277). Mirrors the field on
    /// `EmailEnvelope`; surfaced here so reply / forward flows
    /// have it without an extra cache lookup.  `None` for older
    /// cached payloads or when the source mail had no
    /// Message-ID at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// RFC 5322 `In-Reply-To:` (#277).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// RFC 5322 `References:` parsed into ordered ancestor IDs
    /// (#277).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references_ids: Vec<String>,
    /// Cryptographic protection detected on this message (#57).
    /// Kebab-case string form of `unkai_crypto::Protection`:
    /// `"signed" | "encrypted" | "signed-and-encrypted"`.  Stored
    /// as a string rather than the typed enum to keep
    /// `unkai-core` independent of `unkai-crypto` and to match
    /// the existing JSON-over-IPC convention (e.g. `replied_kind`).
    /// `None` = plain message or legacy cache row that pre-dates
    /// this field; the UI renders no chip in either case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protection: Option<String>,
    /// Signature-verification outcome (#57).  Kebab-case string
    /// form of `unkai_crypto::SignatureStatus`:
    /// `"valid" | "invalid" | "unknown-signer"`.  `None` when
    /// the message wasn't signed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_status: Option<String>,
    /// Hex fingerprint of the verified signer (#57).  Only set
    /// when `signature_status == Some("valid")` *and* the
    /// signer's public key was in our trusted set; otherwise
    /// `None`.  Surfaced to the UI as "signed by 9F2A…AAAA"
    /// so the user can compare against the expected sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer_fingerprint: Option<String>,
    /// Local-only pin state (#414).  Mirrors the field on
    /// `EmailEnvelope`; protocol fetch paths always produce
    /// `false` and the cache / IPC layer stamps the stored value.
    #[serde(default)]
    pub is_pinned: bool,
    /// Sender-declared priority from the `X-Priority:` /
    /// `Importance:` headers (#414): `"high"` / `"low"`, `None` =
    /// normal.  Mirrors the field on `EmailEnvelope`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// User-set priority override (#414).  Local-only; mirrors the
    /// field on `EmailEnvelope`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_override: Option<String>,
    /// Pending reminder time in unix-epoch seconds (#415).
    /// Local-only; mirrors the field on `EmailEnvelope`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder_at: Option<i64>,
    /// The sender's `Disposition-Notification-To:` header value
    /// (RFC 8098, #416) — present means "please tell me when this
    /// was read", naming the address the receipt should go to.
    /// Parsed by the full-message fetch paths and persisted on the
    /// envelope row so a cache-served open still knows to offer
    /// the receipt banner.  `None` = no receipt requested (the
    /// overwhelmingly common case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mdn_requested_to: Option<String>,
    /// What the user (or their `Always`/`Never` policy) already did
    /// about this message's receipt request (#416): `"sent"` or
    /// `"declined"`.  Local-only like `is_pinned` — there is no
    /// wire equivalent, protocol clients always produce `None`, and
    /// the cache overlay fills in the stored value.  Drives the
    /// banner's "don't ask twice" behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mdn_handled: Option<String>,
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
    /// `In-Reply-To` value to set on the outgoing message (#277).
    /// Carries the parent's `Message-ID` *without* angle brackets;
    /// the SMTP layer adds them.  `None` for original mails (not
    /// replies); `Some` for any reply / forward where we know
    /// the parent's Message-ID.  This is what makes other
    /// clients thread our reply with its parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// `References` chain to set on the outgoing message (#277).
    /// Ordered oldest-first, *without* angle brackets per element.
    /// Per RFC 5322 §3.6.4 the new message's References should
    /// be the parent's References plus the parent's Message-ID,
    /// so the chain grows by one entry on each reply.  Empty for
    /// original mails.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    /// End-to-end encryption mode for this send (#57).  Kebab-case
    /// string: `"pgp"` triggers the SMTP layer to wrap the built
    /// MIME as RFC 3156 PGP/MIME using the account's signing key
    /// and the recipients' cached public keys.  Future `"smime"`
    /// will trigger RFC 8551 enveloped-data wrapping (#338).
    /// `None` = plaintext, the historical behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_mode: Option<String>,
    /// Sign this message with the sending account's OpenPGP key (#57).
    /// Independent of `encryption_mode`: a message can be signed
    /// without being encrypted (`multipart/signed`), encrypted
    /// without being signed (encrypt-only), or both.  Defaults to
    /// `false` so existing send call sites that don't set it
    /// preserve the historical plaintext behaviour.
    #[serde(default)]
    pub signing_enabled: bool,
    /// Ask recipients for a read receipt (RFC 8098, #416).  When
    /// set, the SMTP layer stamps `Disposition-Notification-To:`
    /// with the sender's own address on the outgoing message, and
    /// the send pipeline records the sent Message-ID so an
    /// incoming `message/disposition-notification` reply can be
    /// matched back to this mail and surfaced as "read" status.
    /// Advisory only — most clients let their user ignore or
    /// decline the request, so absence of a receipt never means
    /// the mail went unread.  `#[serde(default)]` keeps queued
    /// outbox rows from before this field deserialising cleanly.
    #[serde(default)]
    pub request_read_receipt: bool,
}

/// Calendar payload emitted as the iMIP `text/calendar` body
/// alternative.  `method` is the iTIP method (REQUEST / REPLY /
/// CANCEL etc.); `ics` is the full VCALENDAR/VEVENT body
/// `unkai_caldav::ical::build_ics_with_method` produced.
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

/// Which kind of groupware source a [`NextcloudAccount`] record
/// describes (#413).
///
/// Historically the record only ever meant "a Nextcloud server".
/// Contacts/calendars can now also come from a plain CardDAV/CalDAV
/// server on a different host than the mail or Nextcloud account, or
/// from a purely local store with no remote at all. Those sources
/// reuse this record (and every sync command, cache table and view
/// keyed on `nextcloud_account_id`) rather than duplicating the whole
/// pipeline — the `kind` tells each code path which parts apply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DavSourceKind {
    /// A full Nextcloud connection (Login Flow v2, OCS capabilities,
    /// Talk/Files/Notes/… on top of DAV). The default so records
    /// stored before this field existed keep their meaning.
    #[default]
    Nextcloud,
    /// A generic CardDAV/CalDAV server. Only contact/calendar sync
    /// applies; DAV collection homes are stored explicitly in
    /// `carddav_home` / `caldav_home` instead of being derived from
    /// Nextcloud's `/remote.php/dav/...` layout.
    Dav,
    /// No remote at all — contacts/calendars live only in the local
    /// encrypted cache. Sync commands are no-ops; writes skip the
    /// server round-trip and mint synthetic hrefs/etags.
    Local,
}

/// A persistent groupware connection: a Nextcloud server, a generic
/// CardDAV/CalDAV server, or a purely local contact/calendar store
/// (see [`DavSourceKind`]).
///
/// One `NextcloudAccount` can be shared across multiple mail accounts —
/// users often have several email identities but a single Nextcloud
/// instance that backs attachments, Talk rooms, contacts and calendars.
/// That's why this lives as its own top-level record (separate from
/// `Account`) and is not keyed by email.
///
/// The `app_password` itself is **never** stored here — it lives in the
/// OS keychain under service `unkai-mail-nextcloud` keyed by `id`.
/// `capabilities` is cached at connect time so the UI can show which
/// Nextcloud apps are available without a round-trip on every render.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextcloudAccount {
    /// Stable UUID; used as the keychain account key for the app password.
    pub id: String,
    /// Base URL of the server, e.g. `https://cloud.example.com`.
    /// Stored without trailing slash. Empty string for
    /// [`DavSourceKind::Local`] sources (there is no server).
    pub server_url: String,
    /// Login name. For Nextcloud this is what Login Flow v2 returned
    /// (often differs from the user's email); for generic DAV it's
    /// the HTTP Basic username. Empty for local sources.
    pub username: String,
    /// Optional pretty name shown in the UI — pulled from
    /// `/ocs/v2.php/cloud/user` after login when available, or
    /// user-chosen for DAV/local sources.
    pub display_name: Option<String>,
    /// What the server supports, snapshotted at connect time. For
    /// DAV/local sources this is synthesised from which of
    /// contacts/calendars the user enabled.
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
    /// What this record actually is — see [`DavSourceKind`] (#413).
    /// Defaults to `Nextcloud` so pre-existing rows keep working.
    #[serde(default)]
    pub kind: DavSourceKind,
    /// Absolute CardDAV addressbook-home URL for `kind == Dav`
    /// sources, resolved via RFC 6764 well-known discovery at add
    /// time. `None` for Nextcloud (derived from the server layout)
    /// and local sources.
    #[serde(default)]
    pub carddav_home: Option<String>,
    /// Absolute CalDAV calendar-home URL for `kind == Dav` sources.
    /// Same story as `carddav_home`.
    #[serde(default)]
    pub caldav_home: Option<String>,
}

impl NextcloudAccount {
    /// True when this source has no remote — writes stay in the
    /// local cache and sync is a no-op.
    pub fn is_local(&self) -> bool {
        self.kind == DavSourceKind::Local
    }

    /// True when this source is a real Nextcloud (OCS capabilities,
    /// Talk/Files/Notes endpoints, `/remote.php/dav/...` layout).
    pub fn is_nextcloud(&self) -> bool {
        self.kind == DavSourceKind::Nextcloud
    }
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
    // parser/serialiser in `unkai-carddav` rounds these through
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
/// `unkai-core` rather than inside `unkai-nextcloud` so the
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

/// One Nextcloud Tasks task list (#92).
///
/// Nextcloud Tasks stores tasks as VTODO components inside CalDAV
/// collections.  A "task list" is the same kind of CalDAV
/// collection a calendar lives in — distinguished by the
/// `supported-calendar-component-set` PROPFIND prop including
/// `VTODO`.  The app-side `id` is `{nc_id}::{path}` so the UI can
/// reference a single string while the natural
/// `(nextcloud_account_id, path)` stays the cache key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskList {
    /// Composite app-side id (`{nc_id}::{path}`).
    pub id: String,
    pub nextcloud_account_id: String,
    /// Absolute URL of the CalDAV collection (used for PUT / sync).
    pub path: String,
    /// Last path segment of `path` — stable cache key even if
    /// `display_name` changes server-side.
    pub name: String,
    /// User-facing label from the CalDAV `displayname` prop.
    pub display_name: String,
    /// Hex colour assigned to the collection (`apple:calendar-color`).
    /// Empty when the server didn't advertise one.
    #[serde(default)]
    pub color: Option<String>,
    /// True when the user only has read access on this list — the
    /// editor hides the add / edit / delete affordances when set.
    #[serde(default)]
    pub read_only: bool,
    /// Local-only flag — `true` removes the task list from the
    /// TasksView sidebar AND drops its tasks from the All / Today
    /// / Overdue / Completed virtual buckets (#92).  Mirrors the
    /// per-calendar `hidden` toggle in `Calendar` so the user can
    /// declutter without unsubscribing from the underlying CalDAV
    /// collection.  Never synced to the server — purely a client
    /// preference.
    #[serde(default)]
    pub hidden: bool,
    /// Layer-2 local toggle.  `true` keeps the list in the
    /// TasksView sidebar (greyed out) but suppresses its tasks
    /// from the All / Today / Overdue / Completed virtuals —
    /// same shape as `Calendar::muted` (which keeps a calendar in
    /// the sidebar but stops its events from painting on the
    /// grid).  Controlled by clicking the row's color swatch.
    #[serde(default)]
    pub muted: bool,
}

/// One Nextcloud Tasks task (#92).
///
/// Mirrors the subset of RFC 5545 VTODO fields we surface in the
/// UI.  The task's CalDAV UID is the natural key; `task_list_id`
/// is the composite id of the owning `TaskList` so the cache can
/// keep tasks from many lists in one table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// VTODO `UID` — globally unique per RFC 5545.  Carries the
    /// suffix `.ics` is *not* part of this value; the on-server
    /// href is `{task_list_path}/{uid}.ics`.
    pub uid: String,
    /// Composite app-side id of the owning task list.
    pub task_list_id: String,
    /// Absolute URL of the calendar object resource (used for
    /// PUT / DELETE with `If-Match`).
    pub href: String,
    /// Server etag — sent back on update so a concurrent edit
    /// surfaces as 412 instead of silent overwrite.
    pub etag: String,
    /// VTODO `SUMMARY` — the task's headline.
    pub summary: String,
    /// VTODO `DESCRIPTION` — multi-line body.  `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// VTODO `STATUS` — one of `NEEDS-ACTION` / `IN-PROCESS` /
    /// `COMPLETED` / `CANCELLED`.  Stored as the raw RFC token so
    /// CalDAV round-trips losslessly; the UI maps to a checkbox +
    /// strikethrough.  Defaults to `NEEDS-ACTION` when absent.
    #[serde(default)]
    pub status: String,
    /// VTODO `PRIORITY` (RFC 5545 §3.8.1.9) — integer 1..=9 where
    /// 1 is highest, 5 is medium, 9 is lowest, and 0 / absent means
    /// "no priority".  We map 1..=4 to "high", 5 to "medium",
    /// 6..=9 to "low" in the UI.  Round-trips the raw number so an
    /// external client's exact value survives an edit cycle.
    #[serde(default)]
    pub priority: u8,
    /// VTODO `DUE` — when the task is due, as a UTC instant.  All-
    /// day dues are normalised to midnight UTC; the UI re-renders
    /// in the user's locale.  `None` for tasks without a deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<DateTime<Utc>>,
    /// VTODO `COMPLETED` — when the user marked it done.  Pairs
    /// with `status == COMPLETED`; the writer keeps the two in
    /// lockstep so a CalDAV client that only reads one column
    /// still gets the right answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<DateTime<Utc>>,
    /// VTODO `CREATED` — when the task was first created.  Used
    /// as a fallback sort key when `due` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
    /// VTODO `LAST-MODIFIED` — server-stamped on every change.
    /// Used by the list view's "modified" sort and as a tiebreaker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<DateTime<Utc>>,
    /// VTODO `URL` — when the URL is an Unkai mail reference
    /// (`mail://account/folder/uid`), the UI renders a "Source
    /// mail" chip that opens the originating message.  Any other
    /// URL is shown as-is.  `None` when the task has no `URL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `CATEGORIES` tag list (RFC 5545 §3.8.1.2).  Free-form
    /// strings; the UI surfaces them as chips on the task row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// Raw VCALENDAR/VTODO body — kept so the store can re-parse
    /// later without re-syncing, same pattern as `CalendarEvent`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ics_raw: String,
}

impl Task {
    /// True when `status` is `COMPLETED`.  The UI uses this for
    /// the checkbox state and the strikethrough; backed by RFC
    /// 5545 §3.8.1.11 STATUS, not by the presence of `COMPLETED`
    /// (which is a separate timestamp property).
    pub fn is_completed(&self) -> bool {
        self.status.eq_ignore_ascii_case("COMPLETED")
    }
}

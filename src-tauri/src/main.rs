//! Nimbus — a modern mail client with Nextcloud integration.
//!
//! This is the Tauri application entry point. It registers Tauri
//! commands (the IPC bridge between Rust and Svelte) and launches
//! the native window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod badge;
mod geocode;

use nimbus_caldav::{
    BusyKind as CaldavBusyKind, Calendar as CaldavCalendar, RawEvent,
    build_ics as caldav_build_ics, create_calendar as caldav_create_calendar,
    create_event as caldav_create_event, delete_calendar as caldav_delete_calendar,
    delete_event as caldav_delete_event, list_calendars as caldav_list_calendars,
    nc_principal_home as caldav_nc_principal_home,
    probe_calendar_writable as caldav_probe_writable, query_free_busy as caldav_query_free_busy,
    sync_calendar as caldav_sync_calendar, update_calendar as caldav_update_calendar,
    update_event as caldav_update_event,
};
use nimbus_carddav::{
    Addressbook, ParsedVcard, RawContact, build_vcard, create_contact as carddav_create_contact,
    delete_contact as carddav_delete_contact, list_addressbooks, sync_addressbook,
    update_contact as carddav_update_contact,
};
use nimbus_core::NimbusError;
use nimbus_core::models::{
    Account, AppSettings, CalendarEvent, Contact, CustomTheme, Email, EmailEnvelope, EventAttendee,
    EventReminder, Folder, NextcloudAccount, OutgoingEmail,
};
use nimbus_imap::ImapClient;
use nimbus_jmap::JmapClient;
use nimbus_nextcloud::{
    FileEntry, LoginFlowInit, LoginFlowResult, fetch_capabilities, poll_login, start_login,
};
use nimbus_smtp::{SmtpClient, build_outgoing_message};
use nimbus_store::cache::{
    CalendarEventRow, CalendarEventServerHandle, CalendarRow, ContactRow, ContactServerHandle,
    SearchFilters, SearchHit, SearchScope, SyncState,
};
use nimbus_store::{
    Cache, account_store, app_settings, credentials, link_check, nextcloud_store, settings_bundle,
    settings_sync,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, UriSchemeContext, WindowEvent};
use tokio::sync::RwLock;

/// Shared, mutable app preferences. Held as Tauri managed state so the
/// background loop can snapshot under a read lock on every tick while
/// `update_app_settings` swaps in a new value under the write lock.
type SharedSettings = Arc<RwLock<AppSettings>>;

/// Minimum enforced sync interval — guards against a hand-edited
/// `app_settings.json` DOSing the user's mail server.
const MIN_SYNC_INTERVAL_SECS: u64 = 30;

/// Raw RGBA of the *current* tray base icon — i.e. the icon the
/// badge renderer overlays the unread count onto.  Wrapped in a
/// `Mutex` so `set_logo_style` can hot-swap the bitmap when the
/// user picks a different style without restarting the app.
struct TrayBaseIcon(std::sync::Mutex<TrayBaseIconBitmap>);

#[derive(Clone)]
struct TrayBaseIconBitmap {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

/// Bytes of every per-style logo PNG, baked into the binary at
/// compile time so the picker doesn't depend on runtime resources.
/// 256 px is the right pick: large enough that downscales for the
/// 32 px tray and the 16/32 px Windows window icon stay sharp,
/// small enough that all 7 styles together add < 100 KB to the
/// binary.
mod logo_assets {
    pub const STORM: &[u8] = include_bytes!("../../logos/nimbus-logo/png/storm/nimbus-256.png");
    pub const DAWN: &[u8] = include_bytes!("../../logos/nimbus-logo/png/dawn/nimbus-256.png");
    pub const MINT: &[u8] = include_bytes!("../../logos/nimbus-logo/png/mint/nimbus-256.png");
    pub const SKY: &[u8] = include_bytes!("../../logos/nimbus-logo/png/sky/nimbus-256.png");
    pub const TWILIGHT: &[u8] =
        include_bytes!("../../logos/nimbus-logo/png/twilight/nimbus-256.png");
    pub const MONO_BLACK: &[u8] =
        include_bytes!("../../logos/nimbus-logo/png/monochrome/nimbus-mono-black.png");
    pub const MONO_WHITE: &[u8] =
        include_bytes!("../../logos/nimbus-logo/png/monochrome/nimbus-mono-white.png");

    // ── v2 logo set (added in #197 follow-up) ────────────────────
    // Same 256 px naming convention as v1; lives under the
    // separate `nimbus-logo-v2` folder so the original art and
    // the new pack stay independently swappable.
    pub const COPPER: &[u8] =
        include_bytes!("../../logos/nimbus-logo-v2/png/copper/nimbus-256.png");
    pub const FOREST: &[u8] =
        include_bytes!("../../logos/nimbus-logo-v2/png/forest/nimbus-256.png");
    pub const MIDNIGHT: &[u8] =
        include_bytes!("../../logos/nimbus-logo-v2/png/midnight/nimbus-256.png");
    pub const OCEAN: &[u8] = include_bytes!("../../logos/nimbus-logo-v2/png/ocean/nimbus-256.png");
    pub const ROSE: &[u8] = include_bytes!("../../logos/nimbus-logo-v2/png/rose/nimbus-256.png");
    pub const SLATE: &[u8] = include_bytes!("../../logos/nimbus-logo-v2/png/slate/nimbus-256.png");
    pub const SUNSET: &[u8] =
        include_bytes!("../../logos/nimbus-logo-v2/png/sunset/nimbus-256.png");
}

/// Map a style slug to the embedded PNG bytes.  Unknown slug →
/// fall back to storm so a stray value (mistyped settings file,
/// future-renamed style) can never leave the tray with no icon.
fn logo_bytes_for(style: &str) -> &'static [u8] {
    match style {
        // v1 styles (atmospheric set)
        "dawn" => logo_assets::DAWN,
        "mint" => logo_assets::MINT,
        "sky" => logo_assets::SKY,
        "twilight" => logo_assets::TWILIGHT,
        "monochrome-black" => logo_assets::MONO_BLACK,
        "monochrome-white" => logo_assets::MONO_WHITE,
        // v2 styles (elemental set)
        "copper" => logo_assets::COPPER,
        "forest" => logo_assets::FOREST,
        "midnight" => logo_assets::MIDNIGHT,
        "ocean" => logo_assets::OCEAN,
        "rose" => logo_assets::ROSE,
        "slate" => logo_assets::SLATE,
        "sunset" => logo_assets::SUNSET,
        _ => logo_assets::STORM,
    }
}

/// Decode a PNG into the raw RGBA + dims that Tauri's
/// `tauri::image::Image::new` and our badge compositor both want.
/// Reuses Tauri's bundled PNG decoder so we don't pull a separate
/// `image` crate just for this.
fn decode_logo_png(bytes: &[u8]) -> Result<TrayBaseIconBitmap, NimbusError> {
    let img = tauri::image::Image::from_bytes(bytes)
        .map_err(|e| NimbusError::Other(format!("failed to decode logo PNG: {e}")))?;
    Ok(TrayBaseIconBitmap {
        rgba: img.rgba().to_vec(),
        width: img.width(),
        height: img.height(),
    })
}

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
fn install_notification_icon() -> Result<std::path::PathBuf, NimbusError> {
    // The file is the bundled app icon (a static asset, identical
    // for all installs).  Predictable name in the per-user temp
    // dir is intentional — Windows' notification API needs a
    // stable on-disk path to reference.  No secret data here.
    // nosemgrep: rust.lang.security.temp-dir.temp-dir
    let dir = std::env::temp_dir().join("nimbus-mail");
    std::fs::create_dir_all(&dir)
        .map_err(|e| NimbusError::Other(format!("notification icon mkdir failed: {e}")))?;
    let path = dir.join("nimbus-mail-icon.png");
    std::fs::write(&path, NOTIFICATION_ICON_PNG)
        .map_err(|e| NimbusError::Other(format!("notification icon write failed: {e}")))?;
    Ok(path)
}

#[tauri::command]
fn get_notification_icon_path(state: State<'_, NotificationIconPath>) -> String {
    state.0.to_string_lossy().into_owned()
}

/// Linux-only: send a desktop notification through libnotify with
/// the `DesktopEntry` + `Category` hints set, so the notification
/// daemon (GNOME Shell / KDE Plasma / mako / dunst) tracks it under
/// our app identity and keeps it in its notification center.
///
/// `tauri-plugin-notification` uses notify-rust under the hood but
/// doesn't expose hint APIs in JS, which left dev-build toasts as
/// "anonymous" — they showed up briefly but weren't kept in the
/// notification history. Wrapping the builder ourselves with the
/// hints set is enough to make them persist.
///
/// Returns `Ok(true)` when the call succeeded so the JS side can
/// fall back to the regular plugin if anything goes wrong (e.g.
/// no notification daemon running).
#[cfg(target_os = "linux")]
#[tauri::command]
fn send_native_notification(
    title: String,
    body: String,
    icon: State<'_, NotificationIconPath>,
) -> Result<bool, NimbusError> {
    use notify_rust::{Hint, Notification};
    let mut n = Notification::new();
    n.summary(&title)
        .body(&body)
        .appname("Nimbus Mail")
        .hint(Hint::DesktopEntry("com.nimbus.mail".to_string()))
        .hint(Hint::Category("email".to_string()));
    let icon_path = icon.0.to_string_lossy();
    if !icon_path.is_empty() {
        n.icon(&icon_path);
    }
    n.show()
        .map(|_| true)
        .map_err(|e| NimbusError::Other(format!("notify-rust failed: {e}")))
}

/// Stub on non-Linux platforms — the JS side is expected to fall
/// back to `sendNotification` from the Tauri plugin when this
/// returns `Ok(false)`. Keeps the JS branch code platform-agnostic
/// without needing to ask the OS layer about the platform.
#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn send_native_notification(_title: String, _body: String) -> Result<bool, NimbusError> {
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
/// identifier (`com.nimbus.mail`) the Tauri config sets so the two
/// stay in lockstep.
#[cfg(windows)]
fn set_app_user_model_id() {
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use windows::core::HSTRING;

    let aumid = HSTRING::from("com.nimbus.mail");
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

// ── Tauri commands ──────────────────────────────────────────────
//
// Each `#[tauri::command]` function becomes callable from the
// Svelte frontend via `invoke("command_name", { args })`.
//
// Tauri serialises the return value as JSON and sends it to the
// frontend. Errors must implement `Serialize` (which NimbusError
// does) so Tauri can send them back as structured error objects.

/// Return all configured accounts.
#[tauri::command]
fn get_accounts(cache: State<'_, Cache>) -> Result<Vec<Account>, NimbusError> {
    account_store::load_accounts(&cache)
}

/// Add a new email account and store its password in the OS keychain.
///
/// The frontend sends an `Account` object plus a `password`. The account
/// metadata lands in the encrypted SQLite cache; the password goes to
/// the OS keychain. Separating them keeps secrets off disk and lets the
/// `accounts` table be inspected without exposing credentials.
#[tauri::command]
fn add_account(
    account: Account,
    password: String,
    cache: State<'_, Cache>,
    notify: State<'_, SettingsSyncNotify>,
) -> Result<(), NimbusError> {
    credentials::store_imap_password(&account.id, &password)?;
    account_store::add_account(&cache, account)?;
    notify.0.notify_one();
    Ok(())
}

/// Remove an account and its stored password.
///
/// Order matters: keychain → cached message data → account record.
/// If any step fails, the remaining state is still consistent with
/// the account being present (the user can retry). The account row
/// is deleted last so the rest of the app's "this account exists"
/// queries stay truthful right up until the cleanup completes.
#[tauri::command]
fn remove_account(
    id: String,
    cache: State<'_, Cache>,
    notify: State<'_, SettingsSyncNotify>,
) -> Result<(), NimbusError> {
    credentials::delete_imap_password(&id)?;
    // Best-effort: a failure here leaves orphaned cache rows but doesn't
    // block account removal. Log and continue.
    if let Err(e) = cache.wipe_account(&id) {
        tracing::warn!("failed to wipe cache for account '{id}': {e}");
    }
    account_store::remove_account(&cache, &id)?;
    notify.0.notify_one();
    Ok(())
}

/// Update an existing account's settings.
#[tauri::command]
fn update_account(
    account: Account,
    cache: State<'_, Cache>,
    notify: State<'_, SettingsSyncNotify>,
) -> Result<(), NimbusError> {
    account_store::update_account(&cache, account)?;
    // #168: any account-metadata edit (signature, folder→emoji
    // overrides, sort order, …) is part of the bundle, so wake
    // the auto-sync worker.  No-ops cleanly when sync is off.
    notify.0.notify_one();
    Ok(())
}

/// Replace the IMAP/SMTP password stored in the OS keychain for
/// an existing account.  Kept separate from `update_account` so
/// the password never has to round-trip through the account
/// metadata struct (which lives in the encrypted SQLite cache).
/// `store_imap_password` overwrites in place, so the same call
/// covers initial setup and rotation.
#[tauri::command]
fn set_account_password(id: String, password: String) -> Result<(), NimbusError> {
    if password.is_empty() {
        return Err(NimbusError::Other("password must not be empty".into()));
    }
    credentials::store_imap_password(&id, &password)
}

/// Pin (or clear) a per-folder icon override for an account.
///
/// Passing `Some(emoji)` sets the override; `None` removes it so the
/// folder falls back through the normal icon-resolution chain
/// (special-use attributes → user keyword rules → 📁). The command
/// loads the full `Account` server-side, mutates just
/// `folder_icon_overrides`, and writes back — cheaper than round-
/// tripping the whole struct through the UI, and avoids the UI
/// having to know every field on `Account` just to change one map
/// entry.
#[tauri::command]
fn set_folder_icon(
    account_id: String,
    folder_name: String,
    icon: Option<String>,
    cache: State<'_, Cache>,
    notify: State<'_, SettingsSyncNotify>,
) -> Result<(), NimbusError> {
    let mut account = load_account(&cache, &account_id)?;
    match icon {
        Some(e) if !e.trim().is_empty() => {
            account
                .folder_icon_overrides
                .insert(folder_name, e.trim().to_string());
        }
        _ => {
            account.folder_icon_overrides.remove(&folder_name);
        }
    }
    account_store::update_account(&cache, account)?;
    notify.0.notify_one();
    Ok(())
}

/// Probe Mozilla autoconfig and DNS SRV records for the email's
/// domain and return any IMAP/SMTP server settings discovered.
/// Used by the AccountSetup wizard to prefill the form so most
/// users only need to type their email + password.
///
/// Returns `Ok(None)` when nothing is found — the wizard falls back
/// to manual entry. `Err` only on argument validation failures
/// (e.g. malformed email); transient network errors during the
/// individual probes are swallowed inside the discovery crate so
/// one flaky route doesn't kill the whole flow.
#[tauri::command]
async fn discover_account_settings(
    email: String,
) -> Result<Option<nimbus_discovery::DiscoveredAccount>, NimbusError> {
    match nimbus_discovery::discover(&email).await {
        Ok(found) => Ok(Some(found)),
        Err(nimbus_discovery::DiscoveryError::NotFound) => Ok(None),
        Err(nimbus_discovery::DiscoveryError::Parse(msg)) => Err(NimbusError::Other(msg)),
        Err(nimbus_discovery::DiscoveryError::Network(msg)) => Err(NimbusError::Network(msg)),
    }
}

/// One cert in a probed chain — DER bytes plus its SHA-256
/// fingerprint formatted for display. The frontend uses `der` to
/// build a `TrustedCert` entry and `sha256` to render the
/// "compare this against your server" prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbedCertEntry {
    der: Vec<u8>,
    sha256: String,
}

/// Shape returned to the UI by [`probe_server_certificate`]. The
/// full chain (leaf first, then intermediates) is round-tripped
/// back so the UI can trust every cert the server presented — not
/// just the leaf. This survives chain reordering and reissues of
/// the leaf under the same intermediate without a re-prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbedCert {
    /// Probed certificates in handshake order (leaf at index 0).
    chain: Vec<ProbedCertEntry>,
    host: String,
}

/// Open a no-verify TLS handshake to a mail server and capture the
/// presented certificate chain. Used by the AccountSetup wizard's
/// "trust this server?" path and AccountSettings' re-trust button:
/// when [`test_connection`] fails because the cert isn't trusted,
/// the UI calls this to get the fingerprints, asks the user, and on
/// confirm passes every DER back into `add_account` /
/// `update_account` as `trusted_certs` entries.
///
/// **Safety**: the captured certs are never used for actual mail
/// traffic — the connection is dropped immediately after the
/// handshake. The user explicitly chooses whether to trust them.
#[tauri::command]
async fn probe_server_certificate(host: String, port: u16) -> Result<ProbedCert, NimbusError> {
    let chain_der = nimbus_imap::probe_server_certificate(&host, port).await?;
    let chain = chain_der
        .into_iter()
        .map(|der| {
            let sha256 = nimbus_core::tls::fingerprint_sha256(&der);
            ProbedCertEntry { der, sha256 }
        })
        .collect();
    Ok(ProbedCert { chain, host })
}

/// Validate IMAP credentials by actually logging in.
///
/// The setup wizard calls this before it asks the store to persist the
/// account — an early TCP/TLS/LOGIN round-trip surfaces wrong hostnames,
/// wrong ports, and bad passwords as a structured `NimbusError` with a
/// specific variant (`Network`, `Auth`, `Protocol`) so the UI can phrase
/// the failure clearly instead of saving a dead account and confusing
/// the user on first fetch.
///
/// The session is immediately torn down — this is a probe, not a real
/// fetch; nothing is cached.
#[tauri::command]
async fn test_connection(
    host: String,
    port: u16,
    username: String,
    password: String,
    trusted_certs: Option<Vec<nimbus_core::models::TrustedCert>>,
) -> Result<String, NimbusError> {
    tracing::info!("Testing IMAP connection to {host}:{port} as {username}");
    let trusted = trusted_certs.unwrap_or_default();
    let client = ImapClient::connect(&host, port, &username, &password, &trusted).await?;
    let _ = client.logout().await;
    Ok(format!("IMAP login to {host}:{port} succeeded"))
}

// ── Nextcloud ───────────────────────────────────────────────────
//
// Nextcloud connections are independent of mail accounts: one user can
// have many mail accounts but a single Nextcloud that backs Talk,
// attachments, calendar and contacts. So these commands live on their
// own command family and their own JSON store.
//
// Auth is via Login Flow v2: the UI opens a browser URL, the user
// authorises, and the UI polls `poll_nextcloud_login` until the server
// returns the app password. Nothing in the app ever sees the real
// password — app passwords are revocable from the NC security page.

/// Begin Login Flow v2 — returns the URL to open in the browser plus a
/// polling handle the UI should use to drive `poll_nextcloud_login`.
#[tauri::command]
async fn start_nextcloud_login(
    server_url: String,
    trusted_certs: Option<Vec<nimbus_core::models::TrustedCert>>,
) -> Result<LoginFlowInit, NimbusError> {
    // Login Flow v2 runs before an account exists locally, so the
    // trust list comes from the in-flight setup wizard (#253) rather
    // than the persisted account record.  Frontend default is an
    // empty list; if the first attempt fails with a TLS error the
    // setup wizard pops a cert-probe prompt, the user confirms, and
    // the wizard re-issues `start_nextcloud_login` with the now-
    // populated list.  An empty list = standard webpki verification.
    let certs = trusted_certs.unwrap_or_default();
    start_login(&server_url, &certs).await
}

/// Poll once for Login Flow v2 completion.
///
/// On success, this stores the app password in the OS keychain, queries
/// the server's capabilities, and persists a `NextcloudAccount` record.
/// The UI then just needs to refresh its `get_nextcloud_accounts` view.
///
/// Return shape matches Login Flow v2's own contract so the UI can
/// distinguish "not yet" (`Ok(None)`) from real errors.
#[tauri::command]
async fn poll_nextcloud_login(
    poll_endpoint: String,
    poll_token: String,
    trusted_certs: Option<Vec<nimbus_core::models::TrustedCert>>,
) -> Result<Option<NextcloudAccount>, NimbusError> {
    // Use the wizard-supplied trust list (#253) for both the polling
    // call and the post-login capabilities probe.  Saved into the
    // account record so every subsequent sync uses the same trust.
    let trust_setup = trusted_certs.unwrap_or_default();
    let Some(LoginFlowResult {
        server,
        login_name,
        app_password,
    }) = poll_login(&poll_endpoint, &poll_token, &trust_setup).await?
    else {
        return Ok(None);
    };

    // Stable id derived from server + user so reconnecting updates
    // in place rather than duplicating. Escapes are unnecessary here —
    // `#` can't appear in a hostname or a reasonable NC login name.
    let id = format!("{server}#{login_name}");

    // Store the app password before persisting the account record: if
    // password storage fails the user gets a fresh error with no dead
    // account record left behind.
    credentials::store_nextcloud_password(&id, &app_password)?;

    // Best-effort capability snapshot. A working login with a broken
    // capabilities endpoint shouldn't block saving the account — we
    // can always refetch later.
    let capabilities =
        match fetch_capabilities(&server, &login_name, &app_password, &trust_setup).await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("capabilities fetch failed, saving without: {e}");
                None
            }
        };

    let account = NextcloudAccount {
        id,
        server_url: server,
        username: login_name,
        display_name: None,
        capabilities,
        // Persist the wizard-time trust list so every subsequent
        // sync hits the server with the same fingerprint pinning.
        // Empty by default for the public-CA case (#253).
        trusted_certs: trust_setup,
    };
    nextcloud_store::upsert_account(global_cache()?, account.clone())?;
    Ok(Some(account))
}

/// List all saved Nextcloud connections.
#[tauri::command]
fn get_nextcloud_accounts() -> Result<Vec<NextcloudAccount>, NimbusError> {
    nextcloud_store::load_accounts(global_cache()?)
}

/// Re-probe `/ocs/v2.php/cloud/capabilities` for one account and
/// persist the fresh snapshot. Called by Settings on mount so newly-
/// installed Nextcloud apps (Office, Talk, …) light up their
/// indicator chip without the user having to disconnect + reconnect.
///
/// Soft-fails: a flaky network or revoked password returns the
/// account's previously-cached capabilities unchanged rather than
/// erroring out the whole settings panel.
#[tauri::command]
async fn refresh_nextcloud_capabilities(nc_id: String) -> Result<NextcloudAccount, NimbusError> {
    let mut account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    match fetch_capabilities(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await
    {
        Ok(caps) => {
            account.capabilities = Some(caps);
            nextcloud_store::upsert_account(global_cache()?, account.clone())?;
        }
        Err(e) => {
            tracing::warn!("refresh_nextcloud_capabilities for {nc_id}: {e}");
        }
    }
    Ok(account)
}

/// Fetch the configured email address of the Nextcloud user owning
/// the given account. This is the same `email` field NC's Mail
/// Provider keys against for iMIP, so it's the right value to use as
/// `ORGANIZER` / CHAIR in calendar invites — making the calendar's
/// owning NC identity (not the user's first IMAP account) drive the
/// organizer line in the editor's attendee list.
///
/// Returns `None` when the user hasn't set an email in Personal info
/// or when the OCS lookup fails — caller should fall back to a
/// reasonable default (e.g. the first mail account).
#[tauri::command]
async fn get_nextcloud_user_email(nc_id: String) -> Result<Option<String>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    match nimbus_nextcloud::user::fetch_current_user(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await
    {
        Ok(profile) => Ok(profile.email),
        Err(e) => {
            tracing::warn!("get_nextcloud_user_email for {nc_id}: {e}");
            Ok(None)
        }
    }
}

/// Remove a Nextcloud connection and its stored app password.
///
/// Does **not** attempt to revoke the app password on the server —
/// that would require the password itself and we want removal to be
/// local-only, fast, and offline-safe. Users who want to fully revoke
/// Replace the per-account TLS trust list (#253).
///
/// Used by the AccountSettings panel's "Trusted certificates" section
/// + the Nextcloud setup wizard's cert-probe prompt: when a TLS
/// handshake fails because the server is using a self-signed cert,
/// the UI calls `probe_server_certificate` to capture the chain, asks
/// the user, and on confirm ships the new fingerprints back through
/// here.  Subsequent OCS / CalDAV / CardDAV / Notes / Talk / Files
/// requests pick the new list up automatically — every protocol-crate
/// API rebuilds its `reqwest::Client` per call from the account's
/// `trusted_certs`.
#[tauri::command]
fn update_nextcloud_account_trusted_certs(
    nc_id: String,
    trusted_certs: Vec<nimbus_core::models::TrustedCert>,
    cache: State<'_, Cache>,
) -> Result<NextcloudAccount, NimbusError> {
    let mut account = load_nextcloud_account(&nc_id)?;
    account.trusted_certs = trusted_certs;
    nextcloud_store::upsert_account(&cache, account.clone())?;
    Ok(account)
}

/// Forget a saved Nextcloud connection. The keychain entry goes too so the user
/// can delete the app password from their NC security settings.
///
/// Also drops cached contacts, calendars, and their DAV sync state for
/// this account; a best-effort failure there is logged but doesn't
/// block removal.
#[tauri::command]
fn remove_nextcloud_account(id: String, cache: State<'_, Cache>) -> Result<(), NimbusError> {
    credentials::delete_nextcloud_password(&id)?;
    if let Err(e) = cache.wipe_nextcloud_contacts(&id) {
        tracing::warn!("failed to wipe contacts for NC account '{id}': {e}");
    }
    if let Err(e) = cache.wipe_nextcloud_calendars(&id) {
        tracing::warn!("failed to wipe calendars for NC account '{id}': {e}");
    }
    if let Err(e) = cache.wipe_notes_for_account(&id) {
        tracing::warn!("failed to wipe notes for NC account '{id}': {e}");
    }
    nextcloud_store::remove_account(&cache, &id)
}

/// Open an arbitrary URL in the system's default browser.
///
/// Used by the Nextcloud login flow to hand the user off to their NC
/// server's login page, which happens outside our webview so the
/// browser can handle any SSO / IdP redirects the user's NC is wired
/// up with (Keycloak, OIDC, SAML, etc.).
#[tauri::command]
fn open_url(url: String) -> Result<(), NimbusError> {
    open::that(&url).map_err(|e| NimbusError::Other(format!("failed to open '{url}': {e}")))
}

// ── Nextcloud Files (browse + download) ────────────────────────
//
// WebDAV is stateless and per-folder: the UI asks for the children of
// a path, gets a listing, and asks again when the user navigates. We
// don't cache the tree — Nextcloud's PROPFIND is cheap, and cached
// listings go stale the moment a co-worker drops a new file in a
// shared folder. The picker lives entirely in memory.

/// List the immediate children of a folder in the user's Nextcloud.
///
/// `path` is relative to the user's root (e.g. `/` or `/Documents`).
/// Returns directories and files mixed, in the order the server sent
/// them — the UI sorts if it wants a particular display order.
#[tauri::command]
async fn list_nextcloud_files(nc_id: String, path: String) -> Result<Vec<FileEntry>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::list_directory(
        &account.server_url,
        &account.username,
        &app_password,
        &path,
        &account.trusted_certs,
    )
    .await
}

/// Download a single file from Nextcloud.
///
/// Returns the raw bytes for the UI to stuff into a compose attachment
/// (or save wherever the caller needs). Large files are held in memory
/// for now — matches how locally-picked attachments work. A streaming
/// path is a separate future issue once compose itself streams.
#[tauri::command]
async fn download_nextcloud_file(nc_id: String, path: String) -> Result<Vec<u8>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::download_file(
        &account.server_url,
        &account.username,
        &app_password,
        &path,
        &account.trusted_certs,
    )
    .await
}

/// Fetch a server-rendered preview thumbnail for a Nextcloud
/// file.  Used by the file picker to render inline thumbnails
/// for image / video rows.  Returns `None` (`Ok(None)`) when the
/// server has no preview for this file (404) so the frontend
/// silently falls back to the typed icon instead of surfacing an
/// error to the user.
#[tauri::command]
async fn nextcloud_file_preview(
    nc_id: String,
    path: String,
    size: Option<u32>,
) -> Result<Option<Vec<u8>>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let s = size.unwrap_or(96);
    match nimbus_nextcloud::fetch_preview(
        &account.server_url,
        &account.username,
        &app_password,
        &path,
        s,
        &account.trusted_certs,
    )
    .await
    {
        Ok(bytes) => Ok(Some(bytes)),
        // The 404 ("no preview available") path is legitimate —
        // surface as None so the picker just shows the icon.
        Err(NimbusError::Nextcloud(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Result of `create_nextcloud_share` — both the public URL (for
/// pasting into the email body) and the share id (for later
/// label updates via `update_nextcloud_share_label`).
#[derive(serde::Serialize)]
struct NextcloudShareResult {
    id: String,
    url: String,
}

/// Create a public share link for a Nextcloud file and return the
/// id + URL.
///
/// The compose UI uses this to insert a "click here to download" link
/// into the email body — a lighter alternative to attaching the bytes
/// for big files or files the recipient might want to re-download.
///
/// - `password`: optional, share is gated behind it on the recipient
///   side. The OCS endpoint enforces the user's configured password
///   policy.
/// - `label`: optional human-readable name for the share, visible
///   in Nextcloud's "Shared with others" list (#91).  Compose passes
///   the recipient string for an audit trail.  Empty / `None` leaves
///   Nextcloud's auto-naming intact.
/// - `permissions`: Nextcloud's permission bitmask
///   (1=read, 2=update, 4=create, 8=delete, 16=share).  The Compose
///   share modal exposes the common combinations as a dropdown.
#[tauri::command]
async fn create_nextcloud_share(
    nc_id: String,
    path: String,
    password: Option<String>,
    label: Option<String>,
    permissions: Option<u8>,
) -> Result<NextcloudShareResult, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let share = nimbus_nextcloud::create_public_share(
        &account.server_url,
        &account.username,
        &app_password,
        &path,
        password.as_deref(),
        label.as_deref(),
        permissions.unwrap_or(nimbus_nextcloud::shares::PERM_READ_ONLY),
        &account.trusted_certs,
    )
    .await?;
    Ok(NextcloudShareResult {
        id: share.id,
        url: share.url,
    })
}

/// Update the human-readable label of an existing Nextcloud share
/// (#91 follow-up).  Compose calls this when the user edits the
/// recipient list after a share link has already been minted —
/// otherwise the audit trail in Nextcloud's "Shared with others"
/// list freezes whatever the recipients were at click time.
#[tauri::command]
async fn update_nextcloud_share_label(
    nc_id: String,
    share_id: String,
    label: String,
) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::update_share_label(
        &account.server_url,
        &account.username,
        &app_password,
        &share_id,
        &label,
        &account.trusted_certs,
    )
    .await
}

/// Delete a Nextcloud public share by id (#193).
///
/// Compose calls this when the user discards a draft after having
/// minted share links via the Nextcloud file picker — without the
/// cleanup, the shares dangle in the user's "Shared with others"
/// list with no associated mail.  Save-draft / send paths leave
/// shares intact (the recipient still needs them).
#[tauri::command]
async fn delete_nextcloud_share(nc_id: String, share_id: String) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::delete_share(
        &account.server_url,
        &account.username,
        &app_password,
        &share_id,
        &account.trusted_certs,
    )
    .await
}

/// Pull every `<a data-nimbus-share-id="…" data-nimbus-share-nc="…">`
/// marker out of an HTML string (#193).  Compose stamps these
/// onto every share-link anchor it inserts into a draft body so
/// the delete-message pipeline can map a `https://…/s/<token>`
/// URL back to the share record we need to tear down — there's
/// no other reliable way to recover the OCS share id from the
/// public URL without a list-and-filter round-trip.
///
/// Robust to attribute order (the two `data-` attributes can
/// appear in either order on the same element) and dedupes
/// repeated entries.  Returns `(nc_id, share_id)` pairs.
///
/// We don't pull in the `regex` crate just for this — the HTML
/// is our own (generated by Compose) so a hand-rolled scan over
/// `<a … >` opening tags is enough and stays zero-dependency.
fn extract_managed_shares(html: &str) -> Vec<(String, String)> {
    use std::collections::HashSet;
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        // Find the next `<a` followed by whitespace, `>` or `/`.
        if bytes[i] != b'<' || bytes[i + 1] != b'a' {
            i += 1;
            continue;
        }
        let after = bytes[i + 2];
        if !(after.is_ascii_whitespace() || after == b'>' || after == b'/') {
            i += 1;
            continue;
        }
        // Walk to the closing `>` (anchor opening tag is short
        // and we don't need to handle escaped quotes inside
        // attribute values for our own-generated body).
        let Some(end_off) = bytes[i..].iter().position(|&b| b == b'>') else {
            break;
        };
        let tag_end = i + end_off + 1;
        // Slice into the *original* html (case-preserved) so
        // captured attribute values keep their original case.
        let tag = &html[i..tag_end];
        let share_id = read_attr(tag, "data-nimbus-share-id");
        let nc_id = read_attr(tag, "data-nimbus-share-nc");
        if let (Some(s), Some(n)) = (share_id, nc_id)
            && !s.is_empty()
            && !n.is_empty()
        {
            let key = (n.clone(), s.clone());
            if seen.insert(key) {
                out.push((n, s));
            }
        }
        i = tag_end;
    }
    out
}

/// Read a `name="value"` attribute out of an HTML opening tag.
/// Tight scan: `name=\"…\"` exact match.  Sufficient because we
/// own every site that emits these markers (Compose's share-link
/// renderer) and the attribute values are alphanumeric ids /
/// UUIDs that never contain a literal `"`.  Returns `None` when
/// the attribute is missing.
fn read_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}

/// Write raw bytes to a local file.
///
/// Used by the attachment Download flow: the frontend opens a native
/// "Save As" dialog (via `tauri-plugin-dialog`), the user picks a
/// destination, and the chosen absolute path + the attachment bytes
/// come back here. We use `std::fs::write` which truncates any file
/// already at that path — the native save dialog already asked the
/// user about overwrites, so we don't need a second confirmation.
#[tauri::command]
async fn save_bytes_to_path(path: String, data: Vec<u8>) -> Result<(), NimbusError> {
    // `write` is synchronous and the payload is typically a few MB — the
    // Tauri command runtime already runs us on a worker thread, so we
    // don't need to spawn_blocking.
    std::fs::write(&path, &data)
        .map_err(|e| NimbusError::Other(format!("Failed to write {path}: {e}")))
}

/// Read a small text file (a settings bundle, typically ~kilobytes)
/// from disk.  Used by the "Import settings" flow (#168): the
/// frontend opens a file picker via `plugin-dialog`, gets back an
/// absolute path, and hands it here for the actual read so we
/// don't need a separate filesystem plugin in `package.json`.
#[tauri::command]
async fn read_text_from_path(path: String) -> Result<String, NimbusError> {
    std::fs::read_to_string(&path)
        .map_err(|e| NimbusError::Other(format!("Failed to read {path}: {e}")))
}

/// Upload raw bytes to a file in the user's Nextcloud.
///
/// The "Save to Nextcloud" action on a received email attachment calls
/// this with `path = <chosen folder>/<attachment filename>`. Existing
/// files at the same path are overwritten — the UI confirms with the
/// user before calling when that might be surprising.
#[tauri::command]
async fn upload_to_nextcloud(
    nc_id: String,
    path: String,
    data: Vec<u8>,
    content_type: Option<String>,
) -> Result<String, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::upload_file(
        &account.server_url,
        &account.username,
        &app_password,
        &path,
        data,
        content_type.as_deref(),
        &account.trusted_certs,
    )
    .await
}

/// Create a new (empty) folder in the user's Nextcloud.
///
/// `path` is the full path of the folder to create, relative to the
/// user's root (e.g. `/Documents/New Folder`). The parent must already
/// exist. The file picker calls this when the user clicks "New folder"
/// inside the currently-open directory; on success the picker re-lists
/// the parent so the new entry shows up.
#[tauri::command]
async fn create_nextcloud_directory(nc_id: String, path: String) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::create_directory(
        &account.server_url,
        &account.username,
        &app_password,
        &path,
        &account.trusted_certs,
    )
    .await
}

// ── Office viewer (issue #65) ────────────────────────────────
//
// Click an Office-compatible attachment in MailView → upload its
// bytes to a per-user temp folder in the user's Nextcloud → return
// the deep-link URL the frontend opens in a Tauri webview window.
// On close, the frontend fires `office_close_attachment` which
// expunges the temp file. A separate `office_sweep_temp` runs at
// connect-time to clean up anything left behind by a crash mid-edit.
//
// Folder layout:
//   /Nimbus Mail/temp/<uuid>-<filename>
//
// The UUID prefix lets concurrent edits coexist without filename
// collisions and gives the sweeper an obvious "is-this-ours" gate
// (only delete files inside the temp folder).

/// Root path for Nimbus's per-user temp area on the user's
/// Nextcloud. Files-app-visible (no leading dot) so the user can
/// recover anything we somehow lose track of, but tucked under our
/// app's branded folder so the home screen stays uncluttered.
const NIMBUS_TEMP_ROOT: &str = "/Nimbus Mail";
const NIMBUS_TEMP_DIR: &str = "/Nimbus Mail/temp";

/// Result of `office_open_attachment` — the URL the frontend opens
/// in a fresh webview window plus the temp path it should pass back
/// to `office_close_attachment` on close.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OfficeOpenResult {
    /// Absolute URL into Nextcloud's Files app, which routes the
    /// file id to whichever app is registered as its handler —
    /// Collabora for Office docs, the PDF viewer for `.pdf`. Pasted
    /// directly into a `WebviewWindow` `url` arg.
    url: String,
    /// Path on the user's Nextcloud (relative to the user root).
    /// Round-trips back to `office_close_attachment` so the cleanup
    /// targets the file we just uploaded, not "all temp files".
    temp_path: String,
}

/// Best-effort `MKCOL` of `/Nimbus Mail` and `/Nimbus Mail/temp`.
/// Both are idempotent: `create_directory` returns "folder already
/// exists" as `NimbusError::Nextcloud` which we swallow so a
/// pre-existing folder doesn't fail the open. Anything else
/// propagates so quota / 401 / network errors surface to the user.
async fn ensure_temp_dir(
    account: &NextcloudAccount,
    app_password: &str,
) -> Result<(), NimbusError> {
    for dir in [NIMBUS_TEMP_ROOT, NIMBUS_TEMP_DIR] {
        match nimbus_nextcloud::create_directory(
            &account.server_url,
            &account.username,
            app_password,
            dir,
            &account.trusted_certs,
        )
        .await
        {
            Ok(()) => {}
            Err(NimbusError::Nextcloud(msg)) if msg.contains("already exists") => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Upload an attachment to the user's Nextcloud temp folder and
/// return the URL to open it in. Used by MailView when the user
/// clicks a `cid:` link or a tray button on an Office-compatible
/// attachment.
#[tauri::command]
async fn office_open_attachment(
    nc_id: String,
    filename: String,
    data: Vec<u8>,
    content_type: Option<String>,
) -> Result<OfficeOpenResult, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    ensure_temp_dir(&account, &app_password).await?;

    // UUID prefix dodges filename collisions between concurrent
    // viewer windows, and gives the sweeper a way to recognise our
    // own files without a metadata round-trip.
    let safe_name = filename.replace(['/', '\\'], "_");
    let temp_path = format!("{}/{}-{}", NIMBUS_TEMP_DIR, uuid::Uuid::new_v4(), safe_name);

    nimbus_nextcloud::upload_file(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        data,
        content_type.as_deref(),
        &account.trusted_certs,
    )
    .await?;

    // Resolve the freshly-uploaded file's `oc:fileid` so we can
    // build the canonical `index.php/f/<id>` deep link. That URL
    // routes through Nextcloud's "open with default app" — Files
    // hands `.docx` etc. to Collabora, `.pdf` to the PDF viewer,
    // so the same code path works for both document types without
    // app-specific URL templating on our side.
    let file_id = nimbus_nextcloud::propfind_fileid(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        &account.trusted_certs,
    )
    .await?;

    let server = account.server_url.trim_end_matches('/');
    let url = format!("{server}/index.php/f/{file_id}");

    Ok(OfficeOpenResult { url, temp_path })
}

/// Delete a temp file the frontend opened earlier. Best-effort:
/// 404 is swallowed by `delete_path`, network blips bubble up but
/// the frontend logs and moves on — leftover files get caught by
/// `office_sweep_temp` at next connect.
#[tauri::command]
async fn office_close_attachment(nc_id: String, temp_path: String) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::delete_path(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        &account.trusted_certs,
    )
    .await
}

/// Result of `pdf_open_attachment`. Mirrors `OfficeOpenResult` so
/// the frontend can treat both viewers identically — same webview-
/// open + cleanup-on-close shape, the only difference is which URL
/// it points at.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PdfOpenResult {
    url: String,
    temp_path: String,
}

/// Open a PDF attachment in Nextcloud's built-in PDF viewer.
/// Same temp-upload + cleanup-on-close machinery as the Office flow:
///
///   - Bytes go to `/Nimbus Mail/temp/<uuid>-<filename>` on the user's
///     Nextcloud.
///   - We use the same `index.php/f/<fileid>` deep link the Office
///     viewer uses; Files routes the fileid to its registered
///     handler, which for `.pdf` is Nextcloud's built-in PDF
///     viewer.
///
/// On `pdf_close_attachment` (or the startup sweep) the temp file
/// is DAV-DELETED so the viewer URL stops resolving once the
/// viewer window closes.
#[tauri::command]
async fn pdf_open_attachment(
    nc_id: String,
    filename: String,
    data: Vec<u8>,
    content_type: Option<String>,
) -> Result<PdfOpenResult, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    ensure_temp_dir(&account, &app_password).await?;

    let safe_name = filename.replace(['/', '\\'], "_");
    let temp_path = format!("{}/{}-{}", NIMBUS_TEMP_DIR, uuid::Uuid::new_v4(), safe_name);

    nimbus_nextcloud::upload_file(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        data,
        content_type.as_deref(),
        &account.trusted_certs,
    )
    .await?;

    let file_id = nimbus_nextcloud::propfind_fileid(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        &account.trusted_certs,
    )
    .await?;
    let server = account.server_url.trim_end_matches('/');
    let url = format!("{server}/index.php/f/{file_id}");

    Ok(PdfOpenResult { url, temp_path })
}

/// DELETE the temp PDF the frontend opened. Same cleanup path as
/// Office — kept as its own command so the frontend's per-viewer
/// dispatch stays straightforward.
#[tauri::command]
async fn pdf_close_attachment(nc_id: String, temp_path: String) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::delete_path(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        &account.trusted_certs,
    )
    .await
}

/// Clean up anything stuck in `/Nimbus Mail/temp` from a previous
/// session — say the user closed Nimbus mid-edit, or `office_close_
/// attachment` errored on the way out. We list the directory and
/// DELETE every entry whose `last_modified` is older than the cutoff,
/// so an in-flight viewer window in another Nimbus instance doesn't
/// have its file pulled out from under it.
#[tauri::command]
async fn office_sweep_temp(nc_id: String) -> Result<u32, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    // If the temp dir doesn't exist yet (fresh install / first
    // attachment click) treat that as "nothing to sweep". Anything
    // else propagates.
    let entries = match nimbus_nextcloud::list_directory(
        &account.server_url,
        &account.username,
        &app_password,
        NIMBUS_TEMP_DIR,
        &account.trusted_certs,
    )
    .await
    {
        Ok(e) => e,
        Err(NimbusError::Nextcloud(msg)) if msg.contains("not found") => return Ok(0),
        Err(e) => return Err(e),
    };

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(1);
    let mut swept = 0u32;
    for entry in entries {
        let stale = entry.modified.map(|t| t < cutoff).unwrap_or(true);
        if !stale {
            continue;
        }
        let target = format!("{NIMBUS_TEMP_DIR}/{}", entry.name);
        match nimbus_nextcloud::delete_path(
            &account.server_url,
            &account.username,
            &app_password,
            &target,
            &account.trusted_certs,
        )
        .await
        {
            Ok(()) => swept += 1,
            Err(e) => tracing::warn!("office_sweep_temp: failed to delete {target}: {e}"),
        }
    }
    if swept > 0 {
        tracing::info!("office_sweep_temp: cleaned {swept} stale file(s)");
    }
    Ok(swept)
}

/// Open an attachment in its OS-default app so the user can print
/// it from there with the app's own print dialog. Used by the
/// "🖨 Open to print…" entry in the attachment dropdown.
///
/// Why this shape: the *generic* OS print dialog (Windows'
/// `PrintDialog`, the WinForms printer chooser) is just a printer
/// picker — it doesn't show the file, and it relies on each
/// file type's `PrintTo` verb being registered (Edge doesn't
/// register PrintTo for PDFs, so calling it for `.pdf` from a
/// fresh Windows install silently fails). The webview-rendered
/// Chromium print preview is brittle inside Tauri's sandbox.
///
/// What works reliably: open the file in its default handler
/// (Edge / Acrobat for PDF, Word for `.docx`, Photos for images,
/// Notepad for text, etc.) and let the user press **Ctrl/Cmd-P**.
/// Each app's own print dialog shows a real preview of the file
/// alongside the printer chooser — strictly better UX than the
/// generic OS dialog. The trade-off is one extra keystroke,
/// which the menu label calls out so the user expects it.
///
/// The temp file is kept for 10 minutes so the user has time
/// to actually print before we clean up.
#[tauri::command]
async fn print_attachment(file_name: String, bytes: Vec<u8>) -> Result<(), NimbusError> {
    // Per-call subdir name is a UUID v4 — not a predictable
    // path, which is exactly what the lint is meant to catch.
    // nosemgrep: rust.lang.security.temp-dir.temp-dir
    let mut dir = std::env::temp_dir();
    dir.push(format!("nimbus-print-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| NimbusError::Other(format!("create print temp dir: {e}")))?;

    // Strip path separators / NUL from the filename so the spooler
    // sees a flat name in our temp dir, not a path traversal.
    let safe_name: String = file_name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            _ => c,
        })
        .collect();
    let safe_name = if safe_name.trim().is_empty() {
        "attachment".to_string()
    } else {
        safe_name
    };
    let mut path = dir.clone();
    path.push(&safe_name);
    std::fs::write(&path, &bytes)
        .map_err(|e| NimbusError::Other(format!("write print temp file: {e}")))?;

    // `open::that_detached` is the cross-platform "default verb"
    // launcher: ShellExecute open on Windows, `open` on macOS,
    // `xdg-open` (and friends) on Linux. `_detached` so we don't
    // hold a child handle the user could orphan by closing Nimbus.
    if let Err(e) = open::that_detached(&path) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(NimbusError::Other(format!(
            "failed to open '{}' for printing: {e}",
            path.display()
        )));
    }

    let cleanup_dir = dir;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        if let Err(e) = tokio::fs::remove_dir_all(&cleanup_dir).await {
            tracing::debug!(
                "print_attachment cleanup: failed to remove {}: {e}",
                cleanup_dir.display()
            );
        }
    });

    Ok(())
}

// ── Nextcloud Talk ──────────────────────────────────────────────
//
// Three commands, mirroring the file/share pattern: each call loads
// the account + app password from local state and forwards to the
// matching `nimbus_nextcloud::talk::*` function. We don't cache the
// room list — Talk's `/room` is cheap and unread counts go stale the
// moment a colleague sends a message anyway. The sidebar polls on a
// timer instead.

/// List every Talk room the connected Nextcloud user is a participant
/// of. Drives the sidebar's "Talk Rooms" group.
#[tauri::command]
async fn list_talk_rooms(nc_id: String) -> Result<Vec<nimbus_nextcloud::TalkRoom>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::list_rooms(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await
}

/// Create a new group Talk room and invite `participants` to it.
///
/// `participants` carries a tagged enum (`{kind: "user"|"email", value: ...}`)
/// per invitee — `kind=email` triggers Talk's guest-invite flow so
/// recipients without a Nextcloud account get an emailed link. The
/// frontend builds this list from the email's To/Cc by treating
/// addresses matching the connected NC server's user list as `user`
/// and the rest as `email`. (For the MVP we always send `email` and
/// let Talk match users on the server side.)
#[tauri::command]
// `object_type` / `object_id` mirror Nextcloud Calendar's "Make
// it a Talk conversation" flow — pass `objectType: "event"` plus
// any random unique id to have Talk categorise the room as a
// meeting room.  Plain Compose-side "create Talk room" flows
// leave both `None`.
//
// `room_type` controls who can join: `2` = group/private (NC
// users only), `3` = public (anyone with the URL joins as
// guest).  Event-bound rooms default to `3` so externals
// invited via the calendar invite can click through without
// hitting the NC login wall.
async fn create_talk_room(
    nc_id: String,
    room_name: String,
    participants: Vec<nimbus_nextcloud::ParticipantSource>,
    object_type: Option<String>,
    object_id: Option<String>,
    room_type: Option<u8>,
) -> Result<nimbus_nextcloud::TalkRoom, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::create_room(
        &account.server_url,
        &account.username,
        &app_password,
        &room_name,
        &participants,
        nimbus_nextcloud::CreateRoomOptions {
            room_type,
            object_type: object_type.as_deref(),
            object_id: object_id.as_deref(),
        },
        &account.trusted_certs,
    )
    .await
}

/// Surgical PARTSTAT update for an event already in the user's
/// cache — the EventEditor's RSVP dropdown lands here when an
/// attendee changes their response on a meeting that's already
/// on the calendar.
///
/// Why we don't just route this through `update_calendar_event`:
/// regenerating the VEVENT body from form fields drops X-* lines
/// and re-orders properties, which Sabre's iTIP broker reads as
/// a "noisy" diff and silently suppresses the REPLY iMIP.  The
/// inbox card's `respond_to_invite` already implements the
/// byte-preserving surgical path; this command is a thin wrapper
/// that pulls the cached `ics_raw` for an existing event id and
/// hands it to `respond_to_invite` so the same flow applies.
#[tauri::command]
async fn rsvp_existing_event(
    event_id: String,
    partstat: String,
    attendee_hint: Option<String>,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let handle = load_event_handle(&cache, &event_id)?;
    let calendar_id = handle.calendar_id.clone();
    let raw_ics = handle.ics_raw.clone();
    respond_to_invite(calendar_id, raw_ics, partstat, attendee_hint, cache).await
}

/// Toggle a Talk room's public/private visibility.  Used by
/// the EventEditor save flow to downgrade a room from public
/// to private once we've confirmed every attendee is an
/// internal NC user — the externals-only flag is no longer
/// needed and the room shouldn't be join-by-URL after that
/// point.
#[tauri::command]
async fn set_talk_room_public(
    nc_id: String,
    room_token: String,
    public: bool,
) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::set_room_public(
        &account.server_url,
        &account.username,
        &app_password,
        &room_token,
        public,
        &account.trusted_certs,
    )
    .await
}

/// Look up a Nextcloud user by email address.  Returns the
/// matching userId + display name when the address is registered
/// against an NC principal on this server, or `None` otherwise.
/// Used by the EventEditor's chip badge ("internal" pill on
/// attendees who are NC users) and by the Talk participant-add
/// path (internal users get added as `users` source for an
/// in-NC notification, externals get added as `emails` source
/// so Talk emails them a guest URL).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NextcloudUserLookup {
    user_id: String,
    display_name: String,
}
#[tauri::command]
async fn find_nextcloud_user_by_email(
    nc_id: String,
    email: String,
) -> Result<Option<NextcloudUserLookup>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let m = nimbus_nextcloud::find_user_by_email(
        &account.server_url,
        &account.username,
        &app_password,
        &email,
        &account.trusted_certs,
    )
    .await?;
    Ok(m.map(|m| NextcloudUserLookup {
        user_id: m.user_id,
        display_name: m.display_name,
    }))
}

/// Promote an `Email`-source participant to a `User`-source one
/// whenever the address belongs to a real Nextcloud account on
/// this server (issue #124).  The internal user lands in the
/// room as themselves with an in-NC notification instead of
/// receiving a guest invite link via email — better UX, native
/// rights, and no second mail in the recipient's inbox.
///
/// Lookup is fail-soft: a network blip or an admin-restricted
/// sharees endpoint falls through to the original `Email`
/// source so the invite still gets out, just as a guest.  An
/// in-batch cache (`HashMap<lowercased-addr, ParticipantSource>`)
/// keeps duplicate addresses across the To/Cc list to a single
/// OCS round-trip.
async fn promote_email_to_user_if_internal(
    server_url: &str,
    username: &str,
    app_password: &str,
    src: &nimbus_nextcloud::ParticipantSource,
    cache: &mut std::collections::HashMap<String, nimbus_nextcloud::ParticipantSource>,
) -> nimbus_nextcloud::ParticipantSource {
    use nimbus_nextcloud::ParticipantSource;
    let addr = match src {
        ParticipantSource::User(_) => return src.clone(),
        ParticipantSource::Email(a) => a,
    };
    let key = addr.to_lowercase();
    if let Some(hit) = cache.get(&key) {
        return hit.clone();
    }
    let resolved =
        match nimbus_nextcloud::find_user_by_email(server_url, username, app_password, addr, &[])
            .await
        {
            Ok(Some(m)) => ParticipantSource::User(m.user_id),
            Ok(None) => src.clone(),
            Err(e) => {
                tracing::warn!(
                    "talk-invite: NC user lookup failed for {addr}: {e}; \
                 falling back to email guest"
                );
                src.clone()
            }
        };
    cache.insert(key, resolved.clone());
    resolved
}

/// Add a single participant to an existing Talk room. Exposed so the
/// UI can grow an "Add participant" affordance later without a
/// backend round-trip.  Email-source participants whose address
/// matches a Nextcloud user on this server are silently promoted
/// to `User` source (issue #124).
#[tauri::command]
async fn add_talk_participant(
    nc_id: String,
    room_token: String,
    participant: nimbus_nextcloud::ParticipantSource,
) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let mut cache = std::collections::HashMap::new();
    let resolved = promote_email_to_user_if_internal(
        &account.server_url,
        &account.username,
        &app_password,
        &participant,
        &mut cache,
    )
    .await;
    nimbus_nextcloud::add_participant(
        &account.server_url,
        &account.username,
        &app_password,
        &room_token,
        &resolved,
        &account.trusted_certs,
    )
    .await
}

/// Batched add — invite a whole list of participants on a single
/// auth handshake.  Used by Compose's deferred-invite flow (#86):
/// we create the Talk room empty at compose-time and only invite
/// the recipients once `Send` actually goes out, so a discarded
/// draft doesn't leave a room full of strangers in the recipient's
/// Talk list.  Sequential (not parallel) so the first failure halts
/// the batch and surfaces as a single error.  Email-source entries
/// whose address matches a Nextcloud user on this server are
/// promoted to `User` source per issue #124 — internal recipients
/// join natively, externals still get the email-guest flow.
#[tauri::command]
async fn add_talk_participants(
    nc_id: String,
    room_token: String,
    participants: Vec<nimbus_nextcloud::ParticipantSource>,
) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let mut cache = std::collections::HashMap::new();
    for p in &participants {
        let resolved = promote_email_to_user_if_internal(
            &account.server_url,
            &account.username,
            &app_password,
            p,
            &mut cache,
        )
        .await;
        nimbus_nextcloud::add_participant(
            &account.server_url,
            &account.username,
            &app_password,
            &room_token,
            &resolved,
            &account.trusted_certs,
        )
        .await?;
    }
    Ok(())
}

/// Tear down a Talk room (#86).  Compose's `cancel` flow calls this
/// whenever the user discards a draft that minted a room earlier
/// in the session — without it, the room would dangle empty in the
/// user's Talk list with no context.
#[tauri::command]
async fn delete_talk_room(nc_id: String, room_token: String) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::delete_room(
        &account.server_url,
        &account.username,
        &app_password,
        &room_token,
        &account.trusted_certs,
    )
    .await
}

/// Rename an existing Talk room. Used by the Compose "Add Event"
/// flow to keep the auto-created Talk room's name in sync with the
/// final event title once the user saves the event.
#[tauri::command]
async fn rename_talk_room(
    nc_id: String,
    room_token: String,
    new_name: String,
) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::rename_room(
        &account.server_url,
        &account.username,
        &app_password,
        &room_token,
        &new_name,
        &account.trusted_certs,
    )
    .await
}

// ── Nextcloud Notes (issue #67) ────────────────────────────────
//
// Five thin commands wrapping `nimbus_nextcloud::notes`. Same
// shape as the Talk block above: each call loads the chosen NC
// account + app password and forwards. The Notes API is plain
// REST under `/index.php/apps/notes/api/v1/notes`, so there's no
// envelope unpacking — the wire types come straight back.
//
// We deliberately don't cache notes locally: the Notes web UI is
// the canonical editor and we want NotesView to reflect what the
// user just typed there without a sync-roundtrip dance. Cost is
// one HTTP call per list-refresh, which is cheap.

/// Convert the wire-shape `nimbus_nextcloud::Note` (which doesn't
/// know about accounts) into the canonical `nimbus_core::models::Note`
/// we cache and ship to the UI.  Stamping the account id at the
/// boundary keeps the `Note` type a single source of truth across
/// the codebase.
fn nc_note_to_core(nc_id: &str, n: nimbus_nextcloud::Note) -> nimbus_core::models::Note {
    nimbus_core::models::Note {
        id: n.id,
        nextcloud_account_id: nc_id.to_string(),
        etag: n.etag,
        modified: n.modified,
        title: n.title,
        category: n.category,
        content: n.content,
        favorite: n.favorite,
    }
}

/// Cache-first list (#138).  Returns whatever's on disk so the UI
/// paints instantly; the frontend kicks off a background sync via
/// `sync_nextcloud_notes` to refresh.  Mirrors how `get_contacts`
/// and the mail list work.
#[tauri::command]
fn list_nextcloud_notes(
    nc_id: String,
    cache: State<'_, Cache>,
) -> Result<Vec<nimbus_core::models::Note>, NimbusError> {
    cache.list_notes(&nc_id).map_err(Into::into)
}

/// Pull every note from the server, diff against the cache, and
/// persist the result transactionally.  Server-deleted notes
/// disappear from the cache as part of the same delta.  Returns
/// the fresh list so the caller can update its state without a
/// second round-trip through `list_nextcloud_notes`.
#[tauri::command]
async fn sync_nextcloud_notes(
    nc_id: String,
    cache: State<'_, Cache>,
) -> Result<Vec<nimbus_core::models::Note>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let server = nimbus_nextcloud::list_notes(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    let notes: Vec<nimbus_core::models::Note> = server
        .into_iter()
        .map(|n| nc_note_to_core(&nc_id, n))
        .collect();
    cache.apply_notes_delta(&nc_id, &notes)?;
    Ok(notes)
}

/// Fetch a single note from the server (refreshing its etag) and
/// upsert it into the cache.  Used right before an edit lands so a
/// 412 doesn't fire because the user looked at a stale note ages
/// ago.
#[tauri::command]
async fn get_nextcloud_note(
    nc_id: String,
    note_id: u64,
    cache: State<'_, Cache>,
) -> Result<nimbus_core::models::Note, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let server = nimbus_nextcloud::get_note(
        &account.server_url,
        &account.username,
        &app_password,
        note_id,
        &account.trusted_certs,
    )
    .await?;
    let note = nc_note_to_core(&nc_id, server);
    cache.upsert_note(&note)?;
    Ok(note)
}

/// Create a new note. Title can be empty — the server derives it
/// from the first content line in that case, matching the behaviour
/// of the Notes web UI.  Cache-write-through: the server stamps
/// the id + etag, then we persist locally before returning.
#[tauri::command]
async fn create_nextcloud_note(
    nc_id: String,
    title: String,
    content: String,
    category: String,
    cache: State<'_, Cache>,
) -> Result<nimbus_core::models::Note, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let server = nimbus_nextcloud::create_note(
        &account.server_url,
        &account.username,
        &app_password,
        &nimbus_nextcloud::NewNote {
            title: &title,
            content: &content,
            category: &category,
        },
        &account.trusted_certs,
    )
    .await?;
    let note = nc_note_to_core(&nc_id, server);
    cache.upsert_note(&note)?;
    Ok(note)
}

/// Apply a partial update. Each field is optional — the frontend
/// sends only the ones the user touched so a category-only edit
/// doesn't have to round-trip body bytes the user didn't change.
/// Cache-write-through: the server is authoritative on etag /
/// modified; we persist what it returns.
#[tauri::command]
async fn update_nextcloud_note(
    nc_id: String,
    note_id: u64,
    etag: String,
    title: Option<String>,
    content: Option<String>,
    category: Option<String>,
    favorite: Option<bool>,
    cache: State<'_, Cache>,
) -> Result<nimbus_core::models::Note, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let server = nimbus_nextcloud::update_note(
        &account.server_url,
        &account.username,
        &app_password,
        note_id,
        &etag,
        &nimbus_nextcloud::NoteUpdate {
            title: title.as_deref(),
            content: content.as_deref(),
            category: category.as_deref(),
            favorite,
        },
        &account.trusted_certs,
    )
    .await?;
    let note = nc_note_to_core(&nc_id, server);
    cache.upsert_note(&note)?;
    Ok(note)
}

/// Delete a note. Server first (so a 4xx surfaces before we touch
/// local state); cache delete only runs on success so a network
/// failure leaves the user's note intact locally.
#[tauri::command]
async fn delete_nextcloud_note(
    nc_id: String,
    note_id: u64,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    nimbus_nextcloud::delete_note(
        &account.server_url,
        &account.username,
        &app_password,
        note_id,
        &account.trusted_certs,
    )
    .await?;
    cache.delete_note(&nc_id, note_id)?;
    Ok(())
}

// ── CardDAV contacts ────────────────────────────────────────────
//
// Contact sync is driven from a single entry point: the UI calls
// `sync_nextcloud_contacts(nc_id)` (a "Sync now" button in settings,
// or a background tick after login). That command walks the user's
// addressbooks, runs one incremental sync per book via sync-collection
// REPORT, and applies each delta to the local cache transactionally.
//
// The UI never sees hrefs, etags, or sync tokens — it reads fully
// hydrated `Contact` records from the cache via `get_contacts` (list
// view) and `search_contacts` (autocomplete).

/// Summary returned to the UI after a contacts sync run.
///
/// Per-addressbook counts let the UI say something more useful than
/// "done" — e.g. "Contacts: 12 new, 1 removed". `errors` carries the
/// list of addressbooks that failed so the overall sync doesn't look
/// green when one book silently fell over.
#[derive(Debug, Clone, Serialize)]
struct SyncContactsReport {
    nc_account_id: String,
    books_synced: u32,
    upserted: u32,
    deleted: u32,
    errors: Vec<String>,
}

/// Pull the latest contacts from a Nextcloud account.
///
/// Two-step: list addressbooks (PROPFIND on the user's home), then
/// run an incremental sync-collection REPORT against each. Each
/// addressbook's delta is committed to the local cache in its own
/// transaction, so a failure on book N+1 doesn't roll back book N.
/// Per-book errors are logged and accumulated into the report rather
/// than aborting the whole run.
#[tauri::command]
async fn sync_nextcloud_contacts(
    nc_id: String,
    cache: State<'_, Cache>,
) -> Result<SyncContactsReport, NimbusError> {
    let account = nextcloud_store::load_accounts(&cache)?
        .into_iter()
        .find(|a| a.id == nc_id)
        .ok_or_else(|| NimbusError::Other(format!("no Nextcloud account with id '{nc_id}'")))?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    let books = list_addressbooks(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    tracing::info!(
        "CardDAV: {} addressbook(s) to sync for {}",
        books.len(),
        nc_id
    );

    let mut report = SyncContactsReport {
        nc_account_id: nc_id.clone(),
        books_synced: 0,
        upserted: 0,
        deleted: 0,
        errors: Vec::new(),
    };

    for book in books {
        // Prior token (if any) makes the REPORT incremental; missing
        // state means first sync and the CardDAV layer handles that.
        let prev_token = cache
            .get_addressbook_sync_state(&nc_id, &book.name)
            .ok()
            .flatten()
            .and_then(|s| s.sync_token);

        let delta = match sync_addressbook(
            &account.server_url,
            &book.path,
            &account.username,
            &app_password,
            prev_token.as_deref(),
            &account.trusted_certs,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("CardDAV sync failed for book '{}': {e}", book.name);
                report.errors.push(format!("{}: {e}", book.name));
                continue;
            }
        };

        let upserts: Vec<ContactRow> = delta.upserts.iter().map(raw_contact_to_row).collect();

        if let Err(e) = cache.apply_contact_delta(
            &nc_id,
            &book.name,
            book.display_name.as_deref(),
            &upserts,
            &delta.deleted_hrefs,
            delta.new_sync_token.as_deref(),
            book.ctag.as_deref(),
        ) {
            tracing::warn!("apply_contact_delta failed for '{}': {e}", book.name);
            report.errors.push(format!("{}: {e}", book.name));
            continue;
        }

        report.books_synced += 1;
        report.upserted += upserts.len() as u32;
        report.deleted += delta.deleted_hrefs.len() as u32;
    }

    Ok(report)
}

/// Cache-only list of contacts, optionally scoped to a single NC account.
#[tauri::command]
fn get_contacts(
    nc_id: Option<String>,
    cache: State<'_, Cache>,
) -> Result<Vec<Contact>, NimbusError> {
    cache.list_contacts(nc_id.as_deref()).map_err(Into::into)
}

/// Substring search over cached contacts — feeds the compose
/// autocomplete dropdown. `limit` caps the row count so a stray
/// single-character query can't return the whole address book.
#[tauri::command]
fn search_contacts(
    query: String,
    limit: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<Contact>, NimbusError> {
    cache.search_contacts(&query, limit).map_err(Into::into)
}

/// Aggregate sync status for the Settings UI's Contacts and
/// Calendars rows. Both surfaces want the same shape: when did we
/// last successfully sync, and what's the cached count? — so we
/// share the struct and reuse the `SyncStatusRow` Svelte component.
#[derive(Debug, Clone, Serialize)]
struct SyncStatus {
    /// RFC 3339 timestamp of the most recent successful sync across
    /// every addressbook / calendar for this account, or `None` if
    /// the account has never finished one. The frontend formats it
    /// relative ("12m ago" / "Synced just now").
    last_synced_at: Option<String>,
    /// Cached row count for this account (contacts or calendars).
    /// Mostly informational — the row title carries the meaningful
    /// "are we up to date?" signal.
    count: u32,
}

#[tauri::command]
fn get_contacts_sync_status(
    nc_id: String,
    cache: State<'_, Cache>,
) -> Result<SyncStatus, NimbusError> {
    let last = cache
        .latest_addressbook_sync_at(&nc_id)
        .map_err(NimbusError::from)?
        .map(|t| t.to_rfc3339());
    let count = cache.count_contacts(&nc_id).map_err(NimbusError::from)?;
    Ok(SyncStatus {
        last_synced_at: last,
        count,
    })
}

#[tauri::command]
fn get_calendars_sync_status(
    nc_id: String,
    cache: State<'_, Cache>,
) -> Result<SyncStatus, NimbusError> {
    let last = cache
        .latest_calendar_sync_at(&nc_id)
        .map_err(NimbusError::from)?
        .map(|t| t.to_rfc3339());
    let count = cache
        .list_calendars(&nc_id)
        .map(|cs| cs.len() as u32)
        .unwrap_or(0);
    Ok(SyncStatus {
        last_synced_at: last,
        count,
    })
}

/// Fetched separately from `get_contacts` because photo bytes are
/// huge and Tauri serialises them as JSON number arrays — shipping
/// every photo with the list payload made the contacts view feel
/// laggy. The UI requests photos only for rows it actually paints.
#[derive(Debug, Clone, Serialize)]
struct ContactPhoto {
    mime: String,
    data: Vec<u8>,
}

#[tauri::command]
fn get_contact_photo(
    contact_id: String,
    cache: State<'_, Cache>,
) -> Result<Option<ContactPhoto>, NimbusError> {
    Ok(cache
        .get_contact_photo(&contact_id)
        .map_err(NimbusError::from)?
        .map(|(mime, data)| ContactPhoto { mime, data }))
}

/// Field-for-field copy between the CardDAV crate's `RawContact` and
/// the store crate's `ContactRow`. Kept as a free function so neither
/// crate has to depend on the other — the Tauri layer is the only
/// place both are in scope.
fn raw_contact_to_row(c: &RawContact) -> ContactRow {
    ContactRow {
        href: c.href.clone(),
        etag: c.etag.clone(),
        vcard_uid: c.vcard_uid.clone(),
        display_name: c.display_name.clone(),
        emails: c
            .emails
            .iter()
            .map(|e| nimbus_core::models::ContactEmail {
                kind: e.kind.clone(),
                value: e.value.clone(),
            })
            .collect(),
        phones: c
            .phones
            .iter()
            .map(|p| nimbus_core::models::ContactPhone {
                kind: p.kind.clone(),
                value: p.value.clone(),
            })
            .collect(),
        organization: c.organization.clone(),
        photo_mime: c.photo_mime.clone(),
        photo_data: c.photo_data.clone(),
        title: c.title.clone(),
        birthday: c.birthday.clone(),
        note: c.note.clone(),
        addresses: c
            .addresses
            .iter()
            .map(|a| nimbus_core::models::ContactAddress {
                kind: a.kind.clone(),
                street: a.street.clone(),
                locality: a.locality.clone(),
                region: a.region.clone(),
                postal_code: a.postal_code.clone(),
                country: a.country.clone(),
            })
            .collect(),
        urls: c.urls.clone(),
        vcard_raw: c.vcard_raw.clone(),
        kind: c.kind.clone(),
        member_uids: c.member_uids.clone(),
        categories: c.categories.clone(),
    }
}

// ── CardDAV writes (create / update / delete) ───────────────────
//
// These three commands are the UI's entry points for editing
// contacts. They each do the same three-step dance:
//
// 1. Build a vCard 4.0 body from the form input.
// 2. PUT / DELETE against the CardDAV server with the right
//    precondition (If-None-Match for create, If-Match for
//    update/delete) so conflicting writes surface as a structured
//    error rather than silently clobbering remote state.
// 3. Write through to the local cache so the UI reflects the
//    change immediately — we don't wait for the next sync tick.
//
// For update/delete we look up the server bookkeeping (href, etag,
// addressbook) by contact id; the UI never has to carry those around.

/// Editable fields for a contact, shared by create and update.
/// The "extended" block (title, birthday, note, addresses, urls)
/// is optional so older UI versions that don't surface those
/// fields keep working — `update_contact` merges over the cached
/// vCard, so missing fields preserve whatever's on the server
/// instead of clobbering it.
#[derive(Debug, Clone, Deserialize)]
struct ContactInput {
    display_name: String,
    emails: Vec<nimbus_core::models::ContactEmail>,
    phones: Vec<nimbus_core::models::ContactPhone>,
    organization: Option<String>,
    photo_mime: Option<String>,
    photo_data: Option<Vec<u8>>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    birthday: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    addresses: Option<Vec<nimbus_core::models::ContactAddress>>,
    #[serde(default)]
    urls: Option<Vec<String>>,
    // ── #143: vCard 4 fields surfaced in the redesigned form ─────
    #[serde(default)]
    structured_name: Option<nimbus_core::models::StructuredName>,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    anniversary: Option<String>,
    #[serde(default)]
    gender: Option<String>,
    #[serde(default)]
    impp: Option<Vec<nimbus_core::models::ContactImpp>>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    languages: Option<Vec<String>>,
    #[serde(default)]
    geo: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    /// Round-tripped through the vCard's `KEY` property today;
    /// no form UI yet (deferred to a dedicated PGP / X.509 issue).
    #[serde(default)]
    keys: Option<Vec<String>>,
    /// Categories — already stored on Contact / ContactRow but
    /// the form couldn't edit them before #143.  Optional so
    /// callers that don't include the field leave the existing
    /// list intact via the merge logic in `update_contact`.
    #[serde(default)]
    categories: Option<Vec<String>>,
}

/// Create a new contact on Nextcloud and cache it locally.
///
/// `addressbook_url` is the absolute URL of the target book (the
/// `path` field on `Addressbook`). The UI picks it up from the
/// sync report or a dedicated listing command.
///
/// Generates a fresh UUID for the vCard's UID so callers don't
/// have to, and returns the newly cached `Contact` so the UI can
/// slot it straight into its list without re-fetching.
#[tauri::command]
async fn create_contact(
    nc_id: String,
    addressbook_url: String,
    addressbook_name: String,
    input: ContactInput,
    cache: State<'_, Cache>,
) -> Result<Contact, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    let uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let parsed = input_to_parsed(&uid, &input);
    let vcard = build_vcard(&parsed);

    let outcome = carddav_create_contact(
        &account.server_url,
        &addressbook_url,
        &account.username,
        &app_password,
        &uid,
        &vcard,
        &account.trusted_certs,
    )
    .await?;

    let row = parsed_to_row(&outcome.href, &outcome.etag, &uid, &parsed, vcard);
    cache
        .upsert_single_contact(&nc_id, &addressbook_name, &row)
        .map_err(NimbusError::from)?;

    Ok(row_to_contact(&nc_id, &addressbook_name, &row))
}

/// Replace an existing contact on the server with the form's new
/// values. `If-Match` on the cached etag means a concurrent edit
/// on another device surfaces as a 412 (mapped to a readable error)
/// rather than silently overwriting.
#[tauri::command]
async fn update_contact(
    contact_id: String,
    input: ContactInput,
    cache: State<'_, Cache>,
) -> Result<Contact, NimbusError> {
    let handle = load_contact_handle(&cache, &contact_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;

    // Merge the form fields over the existing parsed vCard so fields
    // the edit form doesn't surface (addresses, birthday, urls, note,
    // title, …) round-trip instead of being silently wiped on every
    // edit. The form-editable fields below replace whatever was there.
    let mut parsed = match nimbus_carddav::parse_vcard(&handle.vcard_raw) {
        Ok(p) => p,
        Err(_) => ParsedVcard {
            uid: handle.vcard_uid.clone(),
            ..Default::default()
        },
    };
    parsed.uid = handle.vcard_uid.clone();
    parsed.display_name = input.display_name.clone();
    parsed.emails = input
        .emails
        .iter()
        .map(|e| nimbus_carddav::VcardEmail {
            kind: e.kind.clone(),
            value: e.value.clone(),
        })
        .collect();
    parsed.phones = input
        .phones
        .iter()
        .map(|p| nimbus_carddav::VcardPhone {
            kind: p.kind.clone(),
            value: p.value.clone(),
        })
        .collect();
    parsed.organization = input.organization.clone();
    if input.photo_data.is_some() {
        parsed.photo_mime = input.photo_mime.clone();
        parsed.photo_data = input.photo_data.clone();
    }
    // Extended fields: a UI that surfaces them sends the new value
    // (or `None` to clear); a UI that doesn't sends `Option::None`
    // for the *whole field*, in which case we leave the cached
    // value alone. The distinction is made via `serde(default)` on
    // `ContactInput` — `None` only ever appears when the JSON omits
    // the key entirely, never when the user explicitly cleared it.
    if let Some(t) = &input.title {
        parsed.title = if t.is_empty() { None } else { Some(t.clone()) };
    }
    if let Some(b) = &input.birthday {
        parsed.birthday = if b.is_empty() { None } else { Some(b.clone()) };
    }
    if let Some(n) = &input.note {
        parsed.note = if n.is_empty() { None } else { Some(n.clone()) };
    }
    if let Some(addrs) = &input.addresses {
        parsed.addresses = addrs
            .iter()
            .map(|a| nimbus_carddav::VcardAddress {
                kind: a.kind.clone(),
                street: a.street.clone(),
                locality: a.locality.clone(),
                region: a.region.clone(),
                postal_code: a.postal_code.clone(),
                country: a.country.clone(),
            })
            .collect();
    }
    if let Some(urls) = &input.urls {
        parsed.urls = urls.clone();
    }
    // ── #143: vCard 4 fields ─────────────────────────────────
    // Same merge pattern as the older fields: a `Some` value
    // replaces what's cached (with empty-string clearing the
    // slot for scalar Options), `None` leaves the cached value
    // intact so a UI that doesn't surface the field can still
    // round-trip it.
    if let Some(sn) = &input.structured_name {
        parsed.structured_name = nimbus_carddav::VcardStructuredName {
            family: sn.family.clone(),
            given: sn.given.clone(),
            additional: sn.additional.clone(),
            prefix: sn.prefix.clone(),
            suffix: sn.suffix.clone(),
        };
    }
    if let Some(n) = &input.nickname {
        parsed.nickname = if n.is_empty() { None } else { Some(n.clone()) };
    }
    if let Some(a) = &input.anniversary {
        parsed.anniversary = if a.is_empty() { None } else { Some(a.clone()) };
    }
    if let Some(g) = &input.gender {
        parsed.gender = if g.is_empty() { None } else { Some(g.clone()) };
    }
    if let Some(impp) = &input.impp {
        parsed.impp = impp
            .iter()
            .map(|i| nimbus_carddav::VcardImpp {
                kind: i.kind.clone(),
                value: i.value.clone(),
            })
            .collect();
    }
    if let Some(r) = &input.role {
        parsed.role = if r.is_empty() { None } else { Some(r.clone()) };
    }
    if let Some(langs) = &input.languages {
        parsed.languages = langs.clone();
    }
    if let Some(g) = &input.geo {
        parsed.geo = if g.is_empty() { None } else { Some(g.clone()) };
    }
    if let Some(tz) = &input.timezone {
        parsed.timezone = if tz.is_empty() {
            None
        } else {
            Some(tz.clone())
        };
    }
    if let Some(ks) = &input.keys {
        parsed.keys = ks.clone();
    }
    if let Some(cats) = &input.categories {
        parsed.categories = cats.clone();
    }
    let vcard = build_vcard(&parsed);

    let outcome = carddav_update_contact(
        &handle.href,
        &account.username,
        &app_password,
        &handle.etag,
        &vcard,
        &account.trusted_certs,
    )
    .await?;

    let row = parsed_to_row(
        &outcome.href,
        &outcome.etag,
        &handle.vcard_uid,
        &parsed,
        vcard,
    );
    cache
        .upsert_single_contact(&handle.nextcloud_account_id, &handle.addressbook, &row)
        .map_err(NimbusError::from)?;

    Ok(row_to_contact(
        &handle.nextcloud_account_id,
        &handle.addressbook,
        &row,
    ))
}

/// Delete a contact from the server and the local cache. The
/// server delete is gated on the cached etag; if that fails we
/// leave the cache row alone so the UI can show the user the
/// fresh state on the next sync.
#[tauri::command]
async fn delete_contact(contact_id: String, cache: State<'_, Cache>) -> Result<(), NimbusError> {
    let handle = load_contact_handle(&cache, &contact_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;

    carddav_delete_contact(
        &handle.href,
        &account.username,
        &app_password,
        &handle.etag,
        &account.trusted_certs,
    )
    .await?;

    cache
        .delete_contact_by_id(&contact_id)
        .map_err(NimbusError::from)?;
    Ok(())
}

// ── Reserved Kontaktgruppe (#133 redesign) ────────────────────
//
// Manual mailing lists (KIND:group vCards) are auto-tagged with
// this CATEGORY so iOS / Apple Contacts / NC Contacts surface
// them in a dedicated "Mailing Lists" group.  The
// `list_mailing_lists` IPC filters this exact name out of the
// virtual-row derivation so we don't end up with a circular
// "Mailing Lists" mailing list of mailing lists.
const MAILING_LISTS_CATEGORY: &str = "Mailing Lists";

// ── Categories / Kontaktgruppen (#133 redesign) ──────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContactCategoryView {
    /// CATEGORY name as written on the vCards.
    name: String,
    /// Number of cached contacts carrying this CATEGORY.
    member_count: u32,
    /// True when the user has flipped "Use as mailing list"
    /// off on this category — drives both "no virtual row in
    /// the Mailing Lists tab" and "no autocomplete suggestion".
    use_as_mailing_list: bool,
}

/// Distinct CATEGORIES across every cached contact, with the
/// per-row "use as mailing list" overlay applied.
///
/// First call after the v17 → v18 migration backfills the
/// `categories_json` column from the cached `vcard_raw` for
/// every row whose tag list is still empty.  Idempotent —
/// once a row has a non-empty `categories_json` it's skipped.
#[tauri::command]
fn list_contact_categories(
    cache: State<'_, Cache>,
) -> Result<Vec<ContactCategoryView>, NimbusError> {
    let _ = cache.backfill_categories(|raw| {
        nimbus_carddav::parse_vcard(raw)
            .map(|p| p.categories)
            .unwrap_or_default()
    });
    let cats = cache.list_contact_categories().map_err(NimbusError::from)?;
    let suppressed = cache
        .get_mailing_list_suppressed()
        .map_err(NimbusError::from)?;
    Ok(cats
        .into_iter()
        .filter(|(name, _)| name != MAILING_LISTS_CATEGORY)
        .map(|(name, member_count)| {
            let id = format!("cat:{name}");
            ContactCategoryView {
                use_as_mailing_list: !suppressed.contains(&id),
                name,
                member_count,
            }
        })
        .collect())
}

/// Toggle "use as mailing list" for one Kontaktgruppe.
#[tauri::command]
fn set_category_use_as_mailing_list(
    name: String,
    enabled: bool,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let id = format!("cat:{name}");
    cache
        .set_mailing_list_suppressed(&id, !enabled)
        .map_err(NimbusError::from)
}

/// Add a CATEGORIES tag to one contact's vCard, sync to the
/// server.  Idempotent — a contact already in the category is
/// left alone (no spurious PUT).
#[tauri::command]
async fn add_contact_to_category(
    contact_id: String,
    category: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    rewrite_contact_categories(&contact_id, &cache, |cats| {
        if !cats.iter().any(|c| c == &category) {
            cats.push(category.clone());
            true
        } else {
            false
        }
    })
    .await
}

/// Remove one CATEGORIES tag from a contact's vCard.
#[tauri::command]
async fn remove_contact_from_category(
    contact_id: String,
    category: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    rewrite_contact_categories(&contact_id, &cache, |cats| {
        let before = cats.len();
        cats.retain(|c| c != &category);
        cats.len() != before
    })
    .await
}

/// Rename a category across every contact carrying it.  Loops
/// each tagged contact, rewrites the CATEGORIES list, PUTs.
/// Best-effort per-contact: a failure on one row logs and
/// continues so a flaky network doesn't strand the rename
/// half-applied (the next sync would heal anyway).
#[tauri::command]
async fn rename_contact_category(
    old: String,
    new: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let new = new.trim().to_string();
    if new.is_empty() {
        return Err(NimbusError::Other("new category name is empty".into()));
    }
    let contacts = cache
        .list_contacts_with_category(&old)
        .map_err(NimbusError::from)?;
    for c in contacts {
        if let Err(e) = rewrite_contact_categories_inner(&c.id, &cache, |cats| {
            let mut changed = false;
            for cat in cats.iter_mut() {
                if cat == &old {
                    *cat = new.clone();
                    changed = true;
                }
            }
            if !cats.iter().any(|c| c == &new) {
                cats.push(new.clone());
                changed = true;
            }
            cats.retain(|c| c != &old);
            changed
        })
        .await
        {
            tracing::warn!("rename category on {}: {e}", c.id);
        }
    }
    // Carry the suppressed flag over to the new id so the
    // user's "use as mailing list" choice doesn't reset.
    let suppressed = cache
        .get_mailing_list_suppressed()
        .map_err(NimbusError::from)?;
    if suppressed.contains(&format!("cat:{old}")) {
        cache
            .set_mailing_list_suppressed(&format!("cat:{old}"), false)
            .map_err(NimbusError::from)?;
        cache
            .set_mailing_list_suppressed(&format!("cat:{new}"), true)
            .map_err(NimbusError::from)?;
    }
    // Carry the per-list emoji overlay across the rename too.
    cache
        .rename_mailing_list_setting(&format!("cat:{old}"), &format!("cat:{new}"))
        .map_err(NimbusError::from)?;
    Ok(())
}

/// Delete a category — strips the tag from every contact.  The
/// underlying contacts are untouched, just no longer tagged.
#[tauri::command]
async fn delete_contact_category(name: String, cache: State<'_, Cache>) -> Result<(), NimbusError> {
    let contacts = cache
        .list_contacts_with_category(&name)
        .map_err(NimbusError::from)?;
    for c in contacts {
        if let Err(e) = rewrite_contact_categories_inner(&c.id, &cache, |cats| {
            let before = cats.len();
            cats.retain(|cc| cc != &name);
            cats.len() != before
        })
        .await
        {
            tracing::warn!("delete category on {}: {e}", c.id);
        }
    }
    Ok(())
}

/// Public wrapper that takes a `State<'_, Cache>` and forwards
/// to the private inner — keeps the create/rename/delete IPCs
/// tidy without making them all duplicate the cache extraction.
async fn rewrite_contact_categories<F>(
    contact_id: &str,
    cache: &State<'_, Cache>,
    f: F,
) -> Result<(), NimbusError>
where
    F: FnOnce(&mut Vec<String>) -> bool,
{
    rewrite_contact_categories_inner(contact_id, cache, f).await
}

/// Pull the cached vCard for `contact_id`, mutate its
/// CATEGORIES list via `f`, and PUT the rewritten body back to
/// CardDAV.  Returns early when `f` reports no change so we
/// don't burn a round-trip on a no-op.
async fn rewrite_contact_categories_inner<F>(
    contact_id: &str,
    cache: &Cache,
    f: F,
) -> Result<(), NimbusError>
where
    F: FnOnce(&mut Vec<String>) -> bool,
{
    let handle = load_contact_handle(cache, contact_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;
    let mut parsed = match nimbus_carddav::parse_vcard(&handle.vcard_raw) {
        Ok(p) => p,
        Err(_) => ParsedVcard {
            uid: handle.vcard_uid.clone(),
            ..Default::default()
        },
    };
    parsed.uid = handle.vcard_uid.clone();
    let changed = f(&mut parsed.categories);
    if !changed {
        return Ok(());
    }
    let vcard = build_vcard(&parsed);
    let outcome = carddav_update_contact(
        &handle.href,
        &account.username,
        &app_password,
        &handle.etag,
        &vcard,
        &account.trusted_certs,
    )
    .await?;
    let row = parsed_to_row(
        &outcome.href,
        &outcome.etag,
        &handle.vcard_uid,
        &parsed,
        vcard,
    );
    cache
        .upsert_single_contact(&handle.nextcloud_account_id, &handle.addressbook, &row)
        .map_err(NimbusError::from)?;
    Ok(())
}

// ── Unified mailing lists (#133 redesign) ─────────────────────
//
// Single IPC the Mailing Lists tab + AddressAutocomplete read
// from.  Combines four sources into one flat list:
//   * `cat:<name>`  — a Kontaktgruppe (CATEGORY tag) with
//     `use_as_mailing_list = true`.
//   * `group:<id>`  — an OCS user group.
//   * `team:<id>`   — a Circles / Teams entry.
//   * `list:<uid>`  — a manual KIND:group vCard.
// The reserved `Mailing Lists` category is filtered out so the
// auto-tag we put on every manual list doesn't generate a
// circular row.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MailingListView {
    /// Unified id — see source-prefix list above.
    id: String,
    /// `category` | `nc-group` | `team` | `manual`.  Drives the
    /// pill colour + the CRUD affordances.
    source: String,
    name: String,
    members: Vec<MailingListMemberView>,
    /// Local-only flag — when true the row is suppressed from
    /// AddressAutocomplete.  Categories use the same flag for
    /// the "Use as mailing list" toggle (off → suppressed).
    hidden_from_autocomplete: bool,
    /// Local-only emoji avatar override; `None` falls back to
    /// the source's default icon (🏷️/📨/⚡).
    emoji: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MailingListMemberView {
    display_name: String,
    email: String,
}

/// Build the unified mailing-list view across every source.
/// Read-heavy but cheap — categories are aggregated in one
/// SQL pass and the NC group / team list reuses the existing
/// list_nextcloud_groups path.
#[tauri::command]
async fn list_mailing_lists(cache: State<'_, Cache>) -> Result<Vec<MailingListView>, NimbusError> {
    // Same lazy backfill list_contact_categories does — this
    // IPC is the entry point the autocomplete uses on first
    // launch, before the contacts UI was opened, so we have to
    // rehydrate categories here too or the category-derived
    // rows would surface with zero members.
    let _ = cache.backfill_categories(|raw| {
        nimbus_carddav::parse_vcard(raw)
            .map(|p| p.categories)
            .unwrap_or_default()
    });
    let suppressed = cache
        .get_mailing_list_suppressed()
        .map_err(NimbusError::from)?;
    let emojis = cache.get_mailing_list_emojis().map_err(NimbusError::from)?;
    let mut out: Vec<MailingListView> = Vec::new();

    // 1. Categories.  Skip the reserved one we use as a holder
    // for KIND:group vCards.
    let cats = cache.list_contact_categories().map_err(NimbusError::from)?;
    for (name, _count) in cats {
        if name == MAILING_LISTS_CATEGORY {
            continue;
        }
        let id = format!("cat:{name}");
        // Category rows stay in the Lists tab regardless of
        // the hide flag, so the user can toggle them back on
        // from the same swatch they used to turn them off.
        // The autocomplete client-side filter is what actually
        // suppresses suggestions; the row carries the flag so
        // the UI can render it greyed-out.
        let hidden_from_autocomplete = suppressed.contains(&id);
        let contacts = cache.list_contacts_with_category(&name).unwrap_or_default();
        // Drop members that have no email — a category-derived
        // mailing list is only useful as a sending target, and
        // a row with empty email would just be noise (and
        // would expand to an unaddressable entry in compose
        // autocomplete).  Contacts without email still show
        // up in the Contacts tab under their Contact Group;
        // they only get hidden here in the mailing-list view.
        let members: Vec<MailingListMemberView> = contacts
            .into_iter()
            .filter_map(|c| {
                let email = c
                    .email
                    .into_iter()
                    .next()
                    .map(|e| e.value)
                    .unwrap_or_default();
                if email.is_empty() {
                    None
                } else {
                    Some(MailingListMemberView {
                        display_name: c.display_name,
                        email,
                    })
                }
            })
            .collect();
        let emoji = emojis.get(&id).cloned();
        out.push(MailingListView {
            id,
            source: "category".to_string(),
            name,
            members,
            hidden_from_autocomplete,
            emoji,
        });
    }

    // 2. Manual KIND:group vCards.  These already auto-tag the
    // reserved category so they show up in the Mailing Lists
    // Kontaktgruppe in NC; here we render them directly.
    if let Ok(groups) = cache.list_contact_groups() {
        for g in groups {
            let id = format!("list:{}", g.id);
            let suppressed_row = suppressed.contains(&id);
            let resolved = cache
                .resolve_group_members(&g.nextcloud_account_id, &g.member_uids)
                .unwrap_or_default();
            let members = resolved
                .into_iter()
                .map(|(_id, name, email)| MailingListMemberView {
                    display_name: name,
                    email,
                })
                .collect();
            let emoji = emojis.get(&id).cloned().or_else(|| g.emoji.clone());
            out.push(MailingListView {
                id,
                source: "manual".to_string(),
                name: g.display_name,
                members,
                hidden_from_autocomplete: suppressed_row,
                emoji,
            });
        }
    }

    // 3. Teams.  list_nextcloud_groups already returns OCS
    // user groups + Circles unified under `source = "team"`
    // with cleaned display names — we just forward each row
    // verbatim.  These refresh every call (typically a handful
    // per server, so live OCS round-trip is fine).
    let nc_groups = list_nextcloud_groups(cache).await.unwrap_or_default();
    for g in nc_groups {
        let id = g.id;
        let suppressed_row = suppressed.contains(&id);
        let members = g
            .members
            .into_iter()
            .map(|m| MailingListMemberView {
                display_name: m.display_name,
                email: m.email,
            })
            .collect();
        let emoji = emojis.get(&id).cloned();
        out.push(MailingListView {
            id,
            source: "team".to_string(),
            name: g.display_name,
            members,
            hidden_from_autocomplete: suppressed_row,
            emoji,
        });
    }

    Ok(out)
}

/// Toggle the local hide-from-autocomplete flag for one
/// mailing-list row.  Used by the per-row swatch on
/// non-category rows (manual / NC group / team) — categories
/// use `set_category_use_as_mailing_list` which writes to the
/// same table under the `cat:` id space.
#[tauri::command]
fn set_mailing_list_hidden(
    id: String,
    hidden: bool,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    cache
        .set_mailing_list_suppressed(&id, hidden)
        .map_err(NimbusError::from)
}

/// Set (or clear) the per-list emoji avatar override.  An empty
/// string clears the override so the row falls back to its
/// source icon.  Works for category / manual / team rows alike,
/// keyed by the unified id.
#[tauri::command]
fn set_mailing_list_emoji(
    id: String,
    emoji: Option<String>,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    cache
        .set_mailing_list_emoji(&id, emoji.as_deref().filter(|s| !s.is_empty()))
        .map_err(NimbusError::from)
}

/// Rename a mailing list, dispatched on the unified id prefix.
/// `cat:<name>` rewrites the CATEGORIES tag on every member
/// contact; `list:<uid>` updates the KIND:group vCard's
/// `display_name`.  Teams are read-only and rejected.
#[tauri::command]
async fn rename_mailing_list(
    id: String,
    new_name: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let new_name = new_name.trim().to_string();
    if new_name.is_empty() {
        return Err(NimbusError::Other("new name is empty".into()));
    }
    if let Some(old) = id.strip_prefix("cat:") {
        rename_contact_category(old.to_string(), new_name, cache).await
    } else if let Some(group_id) = id.strip_prefix("list:") {
        // Reuse update_contact_group with the existing member
        // list — passing None for member_uids keeps them intact.
        update_contact_group(group_id.to_string(), Some(new_name), None, cache)
            .await
            .map(|_| ())
    } else {
        Err(NimbusError::Other("teams cannot be renamed".into()))
    }
}

// ── Contact groups / mailing lists (#133, #113) ───────────────
//
// Groups are stored on the server as plain `KIND:group` vCards.
// The CardDAV layer doesn't care — they sync just like
// individuals — so the IPCs here are thin wrappers that build the
// right vCard shape, route writes through the same
// create/update/delete CardDAV path the contacts use, and surface
// the local-only `group_emoji` / `group_hidden` overlay from the
// cache.

/// Snapshot of a group, hydrated for the UI.  `members` is the
/// expanded list of contact rows so the picker / chip strip can
/// render names + first emails without a second round-trip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContactGroupView {
    id: String,
    nextcloud_account_id: String,
    display_name: String,
    member_uids: Vec<String>,
    members: Vec<GroupMemberView>,
    emoji: Option<String>,
    hidden: bool,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GroupMemberView {
    /// Composite contact id (`{nc}::{uid}`) — matches what
    /// `get_contacts` / `search_contacts` already expose.
    id: String,
    display_name: String,
    /// First email address, or empty when the underlying vCard
    /// has none — the UI shows "no email" in that case rather
    /// than failing the expand.
    email: String,
}

/// List every contact group across every connected NC account,
/// each with its members already resolved to (id, name, email)
/// triples so the UI doesn't have to chase referenced UIDs.
#[tauri::command]
fn list_contact_groups(cache: State<'_, Cache>) -> Result<Vec<ContactGroupView>, NimbusError> {
    let groups = cache.list_contact_groups().map_err(NimbusError::from)?;
    let mut out = Vec::with_capacity(groups.len());
    for g in groups {
        let resolved = cache
            .resolve_group_members(&g.nextcloud_account_id, &g.member_uids)
            .map_err(NimbusError::from)?;
        let members = resolved
            .into_iter()
            .map(|(id, display_name, email)| GroupMemberView {
                id,
                display_name,
                email,
            })
            .collect();
        out.push(ContactGroupView {
            id: g.id,
            nextcloud_account_id: g.nextcloud_account_id,
            display_name: g.display_name,
            member_uids: g.member_uids,
            members,
            emoji: g.emoji,
            hidden: g.hidden,
        });
    }
    Ok(out)
}

/// Create a new `KIND:group` vCard on the server and cache it.
/// `member_uids` is the bare-UID list (no `urn:uuid:` prefix);
/// the writer wraps each in the canonical URI form.
#[tauri::command]
async fn create_contact_group(
    nc_id: String,
    addressbook_url: String,
    addressbook_name: String,
    display_name: String,
    member_uids: Vec<String>,
    cache: State<'_, Cache>,
) -> Result<ContactGroupView, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    let uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let parsed = ParsedVcard {
        uid: uid.clone(),
        display_name: display_name.clone(),
        kind: "group".to_string(),
        members: member_uids
            .iter()
            .map(|u| {
                if u.starts_with("urn:uuid:") {
                    u.clone()
                } else {
                    format!("urn:uuid:{u}")
                }
            })
            .collect(),
        // Auto-tag manual mailing lists with the reserved
        // CATEGORY so iOS / NC Contacts surface them in a
        // dedicated Kontaktgruppe.  The list_mailing_lists IPC
        // filters this name out of the virtual-row derivation
        // so we don't end up with a circular "Mailing Lists"
        // mailing list of mailing lists.
        categories: vec![MAILING_LISTS_CATEGORY.to_string()],
        ..Default::default()
    };
    let vcard = build_vcard(&parsed);
    let outcome = carddav_create_contact(
        &account.server_url,
        &addressbook_url,
        &account.username,
        &app_password,
        &uid,
        &vcard,
        &account.trusted_certs,
    )
    .await?;
    let row = parsed_to_row(&outcome.href, &outcome.etag, &uid, &parsed, vcard);
    cache
        .upsert_single_contact(&nc_id, &addressbook_name, &row)
        .map_err(NimbusError::from)?;
    let id = format!("{nc_id}::{uid}");
    Ok(ContactGroupView {
        id,
        nextcloud_account_id: nc_id,
        display_name,
        member_uids,
        members: Vec::new(),
        emoji: None,
        hidden: false,
    })
}

/// Edit an existing group — rename, swap members, both, neither.
/// `display_name` and `member_uids` are optional to keep the IPC
/// usable for partial updates from drag-and-drop (members only)
/// versus the rename modal (name only).
#[tauri::command]
async fn update_contact_group(
    group_id: String,
    display_name: Option<String>,
    member_uids: Option<Vec<String>>,
    cache: State<'_, Cache>,
) -> Result<ContactGroupView, NimbusError> {
    let handle = load_contact_handle(&cache, &group_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;

    let mut parsed = match nimbus_carddav::parse_vcard(&handle.vcard_raw) {
        Ok(p) => p,
        Err(_) => ParsedVcard {
            uid: handle.vcard_uid.clone(),
            ..Default::default()
        },
    };
    parsed.uid = handle.vcard_uid.clone();
    parsed.kind = "group".to_string();
    if let Some(n) = display_name {
        parsed.display_name = n;
    }
    if let Some(uids) = member_uids {
        parsed.members = uids
            .iter()
            .map(|u| {
                if u.starts_with("urn:uuid:") {
                    u.clone()
                } else {
                    format!("urn:uuid:{u}")
                }
            })
            .collect();
    }
    let vcard = build_vcard(&parsed);
    let outcome = carddav_update_contact(
        &handle.href,
        &account.username,
        &app_password,
        &handle.etag,
        &vcard,
        &account.trusted_certs,
    )
    .await?;
    let row = parsed_to_row(
        &outcome.href,
        &outcome.etag,
        &handle.vcard_uid,
        &parsed,
        vcard,
    );
    cache
        .upsert_single_contact(&handle.nextcloud_account_id, &handle.addressbook, &row)
        .map_err(NimbusError::from)?;
    // Re-pull the group with members hydrated so callers can
    // refresh their UI from a single response.
    let groups = cache.list_contact_groups().map_err(NimbusError::from)?;
    let g = groups
        .into_iter()
        .find(|g| g.id == group_id)
        .ok_or_else(|| NimbusError::Other(format!("group '{group_id}' missing after update")))?;
    let resolved = cache
        .resolve_group_members(&g.nextcloud_account_id, &g.member_uids)
        .map_err(NimbusError::from)?;
    Ok(ContactGroupView {
        id: g.id,
        nextcloud_account_id: g.nextcloud_account_id,
        display_name: g.display_name,
        member_uids: g.member_uids,
        members: resolved
            .into_iter()
            .map(|(id, display_name, email)| GroupMemberView {
                id,
                display_name,
                email,
            })
            .collect(),
        emoji: g.emoji,
        hidden: g.hidden,
    })
}

/// Delete a contact group from the server + local cache.
#[tauri::command]
async fn delete_contact_group(
    group_id: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let handle = load_contact_handle(&cache, &group_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;
    carddav_delete_contact(
        &handle.href,
        &account.username,
        &app_password,
        &handle.etag,
        &account.trusted_certs,
    )
    .await?;
    cache
        .delete_contact_by_id(&group_id)
        .map_err(NimbusError::from)?;
    Ok(())
}

/// Local-only "hide this group" toggle — drives the contacts
/// sidebar's hidden state and excludes the group from the
/// AddressAutocomplete search.
#[tauri::command]
fn set_contact_group_hidden(
    group_id: String,
    hidden: bool,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    cache
        .set_contact_group_hidden(&group_id, hidden)
        .map_err(NimbusError::from)
}

/// Local-only emoji avatar overlay for a group.  `None` clears
/// it back to the initials fallback.
#[tauri::command]
fn set_contact_group_emoji(
    group_id: String,
    emoji: Option<String>,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let val = emoji.as_deref().filter(|s| !s.is_empty());
    cache
        .set_contact_group_emoji(&group_id, val)
        .map_err(NimbusError::from)
}

// ── Nextcloud user groups + Teams (#133 follow-up) ────────────
//
// These are *identity / access* groups, separate from the vCard
// `KIND:group` records above.  Members are NC user IDs
// (provisioning-API speak), not vCard UIDs, so the contacts UI
// renders them in their own read-only sections — Nimbus can't
// add or remove members (admin task) but it can surface the
// groups the user already belongs to and resolve their members
// to email addresses for the Compose autocomplete.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NextcloudGroupView {
    /// Nextcloud account this group lives on.
    nextcloud_account_id: String,
    /// Group / circle identifier — used as the picker id; UNIQUE
    /// per (`nextcloud_account_id`, `source`).
    id: String,
    /// `"group"` for OCS user groups, `"team"` for Circles /
    /// Teams.  Rendered as a colored pill in the sidebar.
    source: String,
    display_name: String,
    members: Vec<NextcloudGroupMemberView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NextcloudGroupMemberView {
    user_id: String,
    display_name: String,
    /// Empty when the NC user has no email set in Personal info.
    email: String,
}

/// Strip the SAML / LDAP prefixes some NC instances bake into
/// group ids when they sync from an upstream IdP — the user
/// sees a clean display name instead of `SAML_Engineering`.
/// Idempotent and case-insensitive on the prefix; everything
/// else passes through untouched.
fn humanize_nc_group_name(raw: &str) -> String {
    const PREFIXES: &[&str] = &[
        "SAML_", "saml_", "saml-", "SAML-", "LDAP_", "ldap_", "ldap-", "LDAP-", "OIDC_", "oidc_",
        "oidc-", "OIDC-",
    ];
    for p in PREFIXES {
        if let Some(rest) = raw.strip_prefix(p) {
            return rest.to_string();
        }
    }
    raw.to_string()
}

/// Pull every NC user group and Circle / Team the user belongs
/// to across every connected NC account, hydrating each with
/// (display_name, email) per member.  Soft-fails per group so
/// one restricted group doesn't block the rest.
#[tauri::command]
async fn list_nextcloud_groups(
    cache: State<'_, Cache>,
) -> Result<Vec<NextcloudGroupView>, NimbusError> {
    let accounts = nextcloud_store::load_accounts(&cache).unwrap_or_default();
    let mut out: Vec<NextcloudGroupView> = Vec::new();
    // Build a uid → email fallback map from the local CardDAV
    // cache.  Most NC instances sync the system addressbook into
    // CardDAV with each user's vCard UID == their NC user_id, so
    // this lets us recover emails even when the OCS user-profile
    // endpoint hides them (regular users querying other users
    // only get a display name, not the email field).
    let cache_uid_email: std::collections::HashMap<String, (String, String)> = cache
        .list_contacts(None)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| {
            let email = c
                .email
                .into_iter()
                .next()
                .map(|e| e.value)
                .unwrap_or_default();
            if email.is_empty() {
                return None;
            }
            // Composite id is `nc::uid` — split off the bare UID.
            let uid = c.id.split("::").nth(1).unwrap_or(&c.id).to_string();
            Some((uid, (c.display_name, email)))
        })
        .collect();
    for acc in &accounts {
        let app_password = match credentials::get_nextcloud_password(&acc.id) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("nc-groups: skipping {} ({e})", acc.id);
                continue;
            }
        };
        // OCS user groups -------------------------------------------------
        let group_ids = match nimbus_nextcloud::fetch_my_groups(
            &acc.server_url,
            &acc.username,
            &app_password,
            &acc.trusted_certs,
        )
        .await
        {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("fetch_my_groups failed for {}: {e}", acc.id);
                Vec::new()
            }
        };
        for gid in group_ids {
            let members = collect_group_members(acc, &app_password, &gid, &cache_uid_email).await;
            // OCS groups + Circles both surface as "team" so
            // the UI presents a single Teams section.  We keep
            // the raw `gid` in the unified id (`team:<gid>`) so
            // the per-row hide swatch can still target this
            // exact NC group across reloads.
            out.push(NextcloudGroupView {
                nextcloud_account_id: acc.id.clone(),
                id: format!("team:{gid}"),
                source: "team".to_string(),
                display_name: humanize_nc_group_name(&gid),
                members,
            });
        }
        // Circles / Teams ------------------------------------------------
        let circles = match nimbus_nextcloud::fetch_my_circles(
            &acc.server_url,
            &acc.username,
            &app_password,
            &acc.trusted_certs,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("fetch_my_circles failed for {}: {e}", acc.id);
                Vec::new()
            }
        };
        for c in circles {
            let mids = match nimbus_nextcloud::fetch_circle_member_ids(
                &acc.server_url,
                &acc.username,
                &app_password,
                &c.id,
                &acc.trusted_certs,
            )
            .await
            {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("circle-members failed: {e}");
                    Vec::new()
                }
            };
            let members = resolve_member_profiles(acc, &app_password, mids, &cache_uid_email).await;
            out.push(NextcloudGroupView {
                nextcloud_account_id: acc.id.clone(),
                id: format!("team:{}", c.id),
                source: "team".to_string(),
                display_name: humanize_nc_group_name(&c.display_name),
                members,
            });
        }
    }
    Ok(out)
}

/// Resolve every NC user id in a group to a (display_name,
/// email) tuple via the OCS user-profile endpoint.  Soft-fails
/// individual lookups (a deleted user surfaces with their bare
/// id and an empty email rather than failing the whole call).
async fn collect_group_members(
    acc: &NextcloudAccount,
    app_password: &str,
    group_id: &str,
    cache_uid_email: &std::collections::HashMap<String, (String, String)>,
) -> Vec<NextcloudGroupMemberView> {
    let ids = match nimbus_nextcloud::fetch_group_member_ids(
        &acc.server_url,
        &acc.username,
        app_password,
        group_id,
        &acc.trusted_certs,
    )
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("fetch_group_member_ids({group_id}) failed: {e}");
            return Vec::new();
        }
    };
    resolve_member_profiles(acc, app_password, ids, cache_uid_email).await
}

/// Resolve a list of NC user-ids to (display_name, email) tuples
/// in parallel.  Falls back to the local CardDAV cache (system
/// addressbook) when OCS hides the email field — that's the
/// default for non-admin users querying other accounts.
async fn resolve_member_profiles(
    acc: &NextcloudAccount,
    app_password: &str,
    ids: Vec<String>,
    cache_uid_email: &std::collections::HashMap<String, (String, String)>,
) -> Vec<NextcloudGroupMemberView> {
    let futs = ids.into_iter().map(|uid| async move {
        let prof = nimbus_nextcloud::fetch_user_profile(
            &acc.server_url,
            &acc.username,
            app_password,
            &uid,
            &acc.trusted_certs,
        )
        .await;
        (uid, prof)
    });
    let results = futures::future::join_all(futs).await;
    results
        .into_iter()
        .map(|(uid, prof)| {
            let (display_name, email_from_ocs) = match prof {
                Ok(p) => (p.display_name, p.email.unwrap_or_default()),
                Err(_) => (uid.clone(), String::new()),
            };
            // Fall back to the local CardDAV cache when OCS didn't
            // return an email (regular-user privacy default) — the
            // system addressbook entry usually has it.
            let (display_name, email) = if email_from_ocs.is_empty() {
                match cache_uid_email.get(&uid) {
                    Some((cached_name, cached_email)) => {
                        let dn = if display_name == uid && !cached_name.is_empty() {
                            cached_name.clone()
                        } else {
                            display_name
                        };
                        (dn, cached_email.clone())
                    }
                    None => (display_name, String::new()),
                }
            } else {
                (display_name, email_from_ocs)
            };
            NextcloudGroupMemberView {
                user_id: uid,
                display_name,
                email,
            }
        })
        .collect()
}

/// A trimmed-down addressbook record for the UI's "save new contact
/// to…" dropdown. We don't ship ctags or sync tokens — those are
/// sync-layer bookkeeping the frontend has no business touching.
#[derive(Debug, Clone, Serialize)]
struct AddressbookSummary {
    path: String,
    name: String,
    display_name: Option<String>,
}

/// List the user's addressbooks on a Nextcloud account. Used by
/// the Contacts view to populate a target-addressbook dropdown
/// when creating a new contact. Hits the server (PROPFIND) because
/// the list can change between logins and we want a fresh view.
#[tauri::command]
async fn list_nextcloud_addressbooks(
    nc_id: String,
) -> Result<Vec<AddressbookSummary>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let books: Vec<Addressbook> = list_addressbooks(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    Ok(books
        .into_iter()
        .map(|b| AddressbookSummary {
            path: b.path,
            name: b.name,
            display_name: b.display_name,
        })
        .collect())
}

// ── CalDAV calendars ────────────────────────────────────────────
//
// Calendar sync mirrors the CardDAV flow: one user-facing entry
// point (`sync_nextcloud_calendars`) walks the user's calendars and
// runs an incremental sync-collection REPORT per calendar, persisting
// each delta transactionally via the store. The UI reads cached data
// via `get_cached_calendars` (list for settings / sidebar header) and
// `get_cached_events` (events in a date window — the sidebar body).
//
// What the UI never sees: hrefs, etags, sync tokens, raw ICS blobs.
// Those all stay behind the store boundary.

/// Thin summary of a calendar — what the Svelte side needs to render
/// a row or colour-chip. Sourced from `CachedCalendar` but omits the
/// sync bookkeeping (tokens, ctag) the UI shouldn't care about.
#[derive(Debug, Clone, Serialize)]
struct CalendarSummary {
    id: String,
    nextcloud_account_id: String,
    display_name: String,
    color: Option<String>,
    last_synced_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Layer 1 (Settings). `true` removes the calendar from the sidebar
    /// entirely. Toggled from NextcloudSettings' per-calendar checkboxes.
    #[serde(default)]
    hidden: bool,
    /// Layer 2 (sidebar swatch). `true` keeps the calendar in the sidebar
    /// but stops its events from painting on the agenda grid. Toggled via
    /// the coloured swatch in the CalendarView sidebar.
    #[serde(default)]
    muted: bool,
    /// CalDAV-derived read-only flag (#236).  Mirrors
    /// `current-user-privilege-set`: `true` when the user can't add
    /// or modify events on this calendar (typical for shared
    /// calendars where the owner granted view-only access).  The
    /// EventEditor hides Delete and removes the calendar from the
    /// new-event picker when this is set.
    #[serde(default)]
    read_only: bool,
}

/// Summary returned to the UI after a calendar sync run.
///
/// Per-calendar counts let the UI say "Personal: 4 new, 0 removed"
/// instead of a generic "done". `errors` accumulates per-calendar
/// failures so one broken calendar (commonly a subscribed read-only
/// feed that doesn't support sync-collection) doesn't paint the
/// whole run red.
#[derive(Debug, Clone, Serialize)]
struct SyncCalendarsReport {
    nc_account_id: String,
    calendars_synced: u32,
    upserted: u32,
    deleted: u32,
    errors: Vec<String>,
}

/// Fresh PROPFIND list of the user's calendars on the server.
///
/// Lighter than `sync_nextcloud_calendars` — no per-calendar sync,
/// no cache write. Used in settings UIs where the user just wants
/// to see what calendars exist server-side before toggling sync on.
#[tauri::command]
async fn list_nextcloud_calendars(nc_id: String) -> Result<Vec<CalendarSummary>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let calendars: Vec<CaldavCalendar> = caldav_list_calendars(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    Ok(calendars
        .into_iter()
        .map(|c| CalendarSummary {
            // Matches the id scheme used by the cache — stable across
            // syncs so the UI can key rows by it whether it's looking
            // at a fresh discovery list or the cached list.
            id: format!("{nc_id}::{}", c.path),
            nextcloud_account_id: nc_id.clone(),
            display_name: c.display_name.unwrap_or(c.name),
            color: c.color,
            // Discovery alone doesn't produce a sync timestamp.
            last_synced_at: None,
            // Raw discovery can't know about local toggles — the
            // cache-backed `get_cached_calendars` path does. This
            // command is only used by the setup probe, so defaulting
            // to fully visible is fine.
            hidden: false,
            muted: false,
            // The discovery path has the privilege-set bit; pass
            // it through so the setup probe can already gray out
            // read-only calendars.
            read_only: c.read_only,
        })
        .collect())
}

/// Pull the latest calendars and events from a Nextcloud account.
///
/// Two phases:
///   1. Discovery (PROPFIND) → `upsert_calendars`. This also prunes
///      any calendar that vanished server-side, cascading its events.
///   2. Per-calendar incremental sync. We pass the previous
///      `sync_token` (from the cache) so the server returns only
///      what changed. A failure on calendar N is logged and added
///      to the report; calendar N+1 still runs.
///
/// Each calendar's delta is committed in its own transaction, so
/// a partial run leaves earlier calendars fully up-to-date.
#[tauri::command]
async fn sync_nextcloud_calendars(
    nc_id: String,
    cache: State<'_, Cache>,
) -> Result<SyncCalendarsReport, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    // ── Phase 1: discovery + reconcile the calendar list ────────
    let mut server_calendars = caldav_list_calendars(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    tracing::info!(
        "CalDAV: {} calendar(s) discovered for {}",
        server_calendars.len(),
        nc_id
    );

    // #236 follow-up — privilege-set parsing alone misses a few
    // Sabre/DAV variants (notably some shared-calendar configs
    // that omit `current-user-privilege-set` entirely or
    // advertise write privileges that the actual PUT then
    // refuses with 404).  OPTIONS is similarly unreliable —
    // some configs return the resource type's full method list
    // regardless of ACL.  The only signal that reliably matches
    // what the user hits at save time is an actual PUT, so we
    // do exactly that: drop a placeholder VEVENT in 1970 (so it
    // never collides with real data), DELETE it on the way out,
    // and treat the PUT verdict as canonical.  The probe fires
    // once per calendar per sync — the cost is one extra
    // request pair on top of the existing PROPFIND/REPORT
    // traffic.
    //
    // We OR with the privilege-set verdict so a writable
    // discovery only stands when both signals agree; any
    // probe failure (network, 5xx) leaves the privilege-set
    // verdict alone rather than misclassify on a transient
    // blip.
    for cal in &mut server_calendars {
        match caldav_probe_writable(
            &cal.path,
            &account.username,
            &app_password,
            &account.trusted_certs,
        )
        .await
        {
            Ok(true) => {
                // PUT succeeded → calendar accepts writes. The
                // privilege-set may still have flagged it
                // read-only (rare); we trust the PUT result and
                // *clear* the flag so a re-shared calendar that
                // gets write access back also resurfaces in the
                // editor.
                if cal.read_only {
                    tracing::info!(
                        "CalDAV: write-probe overrides privilege-set on '{}' \
                         (PUT succeeded → marking writable)",
                        cal.path
                    );
                }
                cal.read_only = false;
            }
            Ok(false) => {
                if !cal.read_only {
                    tracing::info!(
                        "CalDAV: write-probe marks calendar '{}' read-only (PUT 403/404)",
                        cal.path
                    );
                }
                cal.read_only = true;
            }
            Err(e) => {
                tracing::warn!(
                    "CalDAV: write-probe for '{}' failed, keeping privilege-set verdict: {e}",
                    cal.path
                );
            }
        }
    }

    let rows: Vec<CalendarRow> = server_calendars
        .iter()
        .map(|c| CalendarRow {
            path: c.path.clone(),
            display_name: c.display_name.clone().unwrap_or_else(|| c.name.clone()),
            color: c.color.clone(),
            ctag: c.ctag.clone(),
            // Fresh inserts default to fully visible; the `upsert_calendars`
            // ON CONFLICT clause leaves `hidden` and `muted` untouched on
            // updates so existing local toggles survive re-sync.
            hidden: false,
            muted: false,
            // #236 — server-side privilege-set + OPTIONS probe agree
            // on whether the editor lets the user write events here.
            // The upsert refreshes this on every discovery so a calendar
            // that gets re-shared as read-only between syncs flips
            // promptly.
            read_only: c.read_only,
        })
        .collect();
    cache.upsert_calendars(&nc_id, &rows)?;

    // ── Phase 2: sync each calendar individually ────────────────
    let mut report = SyncCalendarsReport {
        nc_account_id: nc_id.clone(),
        calendars_synced: 0,
        upserted: 0,
        deleted: 0,
        errors: Vec::new(),
    };

    for cal in server_calendars {
        // id matches the (nc_id, path) scheme `upsert_calendars`
        // just committed, so `get_calendar_sync_state` and
        // `apply_event_delta` will find/target the right row.
        let cal_id = format!("{nc_id}::{}", cal.path);

        let prev_token = cache
            .get_calendar_sync_state(&cal_id)
            .ok()
            .flatten()
            .and_then(|s| s.sync_token);

        let delta = match caldav_sync_calendar(
            &account.server_url,
            &cal.path,
            &account.username,
            &app_password,
            prev_token.as_deref(),
            &account.trusted_certs,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("CalDAV sync failed for calendar '{}': {e}", cal.path);
                report.errors.push(format!("{}: {e}", cal.path));
                continue;
            }
        };

        // One `RawEvent` can carry several VEVENTs (master + overrides
        // at the same href). Flatten into one store row per VEVENT so
        // the range query sees them individually. `ics_raw` is cloned
        // onto every row from the same href — the raw blob stays
        // identical, and the store is optimised for per-row reads,
        // not per-href grouping.
        let upserts: Vec<CalendarEventRow> =
            delta.upserts.iter().flat_map(raw_event_to_rows).collect();

        if let Err(e) = cache.apply_event_delta(
            &cal_id,
            &upserts,
            &delta.deleted_hrefs,
            delta.new_sync_token.as_deref(),
            cal.ctag.as_deref(),
        ) {
            tracing::warn!("apply_event_delta failed for '{}': {e}", cal.path);
            report.errors.push(format!("{}: {e}", cal.path));
            continue;
        }

        report.calendars_synced += 1;
        report.upserted += upserts.len() as u32;
        report.deleted += delta.deleted_hrefs.len() as u32;
    }

    Ok(report)
}

/// Cache-only list of calendars for a Nextcloud account. Used by the
/// sidebar widget on startup so it can paint before the first sync
/// finishes (or if the user is offline).
#[tauri::command]
fn get_cached_calendars(
    nc_id: String,
    cache: State<'_, Cache>,
) -> Result<Vec<CalendarSummary>, NimbusError> {
    let cached = cache.list_calendars(&nc_id)?;
    Ok(cached
        .into_iter()
        .map(|c| CalendarSummary {
            id: c.id,
            nextcloud_account_id: c.nextcloud_account_id,
            display_name: c.display_name,
            color: c.color,
            last_synced_at: c.last_synced_at,
            hidden: c.hidden,
            muted: c.muted,
            read_only: c.read_only,
        })
        .collect())
}

// ── Calendar management commands (Issue #82) ─────────────────
//
// CalDAV wrappers that add / rename / recolor / delete a calendar
// collection on the server and keep the local cache in step. Each
// mutates exactly one calendar row; the next `sync_nextcloud_
// calendars` run reconciles etag / sync-token / event deltas.
// `set_nextcloud_calendar_hidden` is the only one that doesn't
// talk to the server — hidden is a local-only flag.

/// Create a new calendar on the server and seed a cache row.
///
/// The path segment is a fresh UUID so two concurrent creates can't
/// collide on the wire and so a later rename never rewrites URLs
/// downstream (the slug stays stable regardless of display name).
/// Returns the newly-inserted summary so the UI can add it to the
/// sidebar without a follow-up fetch.
#[tauri::command]
async fn create_nextcloud_calendar(
    nc_id: String,
    display_name: String,
    color: Option<String>,
    cache: State<'_, Cache>,
) -> Result<CalendarSummary, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    let server = account.server_url.trim_end_matches('/');
    let home = format!("{server}/remote.php/dav/calendars/{}/", account.username);
    let slug = uuid::Uuid::new_v4().to_string();

    let url = caldav_create_calendar(
        &home,
        &account.username,
        &app_password,
        &slug,
        &display_name,
        color.as_deref(),
        &account.trusted_certs,
    )
    .await?;

    // Seed the cache so the sidebar paints the new calendar
    // instantly. `ctag` / `sync_token` land on the next full sync —
    // no event rows yet anyway, so the bookkeeping gap is cosmetic.
    let row = CalendarRow {
        path: url.clone(),
        display_name: display_name.clone(),
        color: color.clone(),
        ctag: None,
        hidden: false,
        muted: false,
        // The user just created this calendar, so they own it and
        // have full write privileges (#236).  Next discovery cycle
        // confirms via `current-user-privilege-set` PROPFIND.
        read_only: false,
    };
    let id = cache.insert_calendar(&nc_id, &row)?;

    Ok(CalendarSummary {
        id,
        nextcloud_account_id: nc_id,
        display_name,
        color,
        last_synced_at: None,
        hidden: false,
        muted: false,
        // Same reasoning as `insert_calendar` above — fresh
        // user-created calendar is owned by the user, fully
        // writable until next discovery says otherwise.
        read_only: false,
    })
}

/// Rename and/or recolor an existing calendar via a single CalDAV
/// `PROPPATCH`. Either argument may be `None` — passing both `None`
/// is a no-op server-side and cache-side.
#[tauri::command]
async fn update_nextcloud_calendar(
    calendar_id: String,
    display_name: Option<String>,
    color: Option<String>,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let (nc_id, path) = cache
        .get_calendar_server_path(&calendar_id)?
        .ok_or_else(|| NimbusError::Other(format!("no cached calendar with id '{calendar_id}'")))?;
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    caldav_update_calendar(
        &path,
        &account.username,
        &app_password,
        display_name.as_deref(),
        color.as_deref(),
        &account.trusted_certs,
    )
    .await?;

    cache.update_calendar_metadata(&calendar_id, display_name.as_deref(), color.as_deref())?;
    Ok(())
}

/// Delete a calendar on the server + drop the cached row (events
/// cascade). The server's DELETE is destructive and irreversible on
/// most Nextcloud setups — callers (i.e. the UI) are expected to
/// confirm with the user before invoking this.
#[tauri::command]
async fn delete_nextcloud_calendar(
    calendar_id: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let (nc_id, path) = cache
        .get_calendar_server_path(&calendar_id)?
        .ok_or_else(|| NimbusError::Other(format!("no cached calendar with id '{calendar_id}'")))?;
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    caldav_delete_calendar(
        &path,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    cache.remove_calendar(&calendar_id)?;
    Ok(())
}

/// Layer 1: flip a calendar's sidebar visibility. Purely client-side —
/// no CalDAV traffic. `hidden = true` removes the calendar from the
/// sidebar entirely (controlled from NextcloudSettings).
#[tauri::command]
fn set_nextcloud_calendar_hidden(
    calendar_id: String,
    hidden: bool,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    cache.set_calendar_hidden(&calendar_id, hidden)?;
    Ok(())
}

/// Layer 2: flip a calendar's event-grid visibility. Purely client-side.
/// `muted = true` keeps the calendar in the sidebar but stops its events
/// from painting on the agenda grid (controlled via the sidebar swatch).
#[tauri::command]
fn set_nextcloud_calendar_muted(
    calendar_id: String,
    muted: bool,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    cache.set_calendar_muted(&calendar_id, muted)?;
    Ok(())
}

/// Events in `[range_start, range_end)` across the given calendars,
/// with recurring series already expanded into concrete occurrences.
///
/// `calendar_ids` is the full set the UI wants to display at once —
/// typically every calendar belonging to a Nextcloud account, so one
/// round-trip paints the whole sidebar.
///
/// The expansion pipeline:
/// 1. `cache.list_events_for_expansion` returns three buckets of rows
///    — in-window singletons, all recurring masters, all overrides.
///    Masters and overrides are fetched un-windowed because a series'
///    master may predate the window but still have instances inside
///    it, and an override may have been moved from outside the window
///    to inside it (or vice versa).
/// 2. Overrides are indexed by the `{calendar_id}::{uid}` prefix of
///    their composite id — the very same prefix that a master's id
///    has — so matching an override to its series is O(1).
/// 3. `nimbus_caldav::expand_event` does the RFC 5545 work: RRULE
///    enumeration, EXDATE removal, RDATE insertion, override swap-in.
/// Pull events out of the local cache for `calendar_ids` over
/// `[range_start, range_end)`, recurrence-expanded.  Shared by
/// `get_cached_events` (the calendar grid) and
/// `get_attendee_availability` (the planner's local-cache scan
/// for external attendees).
///
/// Mirrors the expansion pipeline documented on `get_cached_events`:
/// singletons + recurring masters + overrides → expand each master
/// against its overrides → sorted chronological list.
fn expand_calendar_events_in_range(
    cache: &Cache,
    calendar_ids: &[String],
    range_start: chrono::DateTime<chrono::Utc>,
    range_end: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<CalendarEvent>, NimbusError> {
    let input = cache
        .list_events_for_expansion(calendar_ids, range_start, range_end)
        .map_err(NimbusError::from)?;

    let mut overrides_by_master: std::collections::HashMap<&str, Vec<&CalendarEvent>> =
        std::collections::HashMap::new();
    for ov in &input.overrides {
        if let Some(master_id) = ov.id.rsplit_once("::").map(|(prefix, _)| prefix) {
            overrides_by_master.entry(master_id).or_default().push(ov);
        }
    }

    let mut out: Vec<CalendarEvent> = input.singletons;
    for master in &input.masters {
        let ovs = overrides_by_master
            .get(master.id.as_str())
            .cloned()
            .unwrap_or_default();
        out.extend(nimbus_caldav::expand_event(
            master,
            &ovs,
            range_start,
            range_end,
        ));
    }
    out.sort_by_key(|e| e.start);
    Ok(out)
}

#[tauri::command]
fn get_cached_events(
    calendar_ids: Vec<String>,
    range_start: chrono::DateTime<chrono::Utc>,
    range_end: chrono::DateTime<chrono::Utc>,
    cache: State<'_, Cache>,
) -> Result<Vec<CalendarEvent>, NimbusError> {
    expand_calendar_events_in_range(&cache, &calendar_ids, range_start, range_end)
}

/// What the Svelte editor sends for a create or update. Matches the
/// `CalendarEvent` shape the UI already knows but flattens to plain
/// strings / booleans the Tauri IPC layer can serialise without
/// extra adapters. Optional fields stay optional so the form can
/// submit a partial event without leaving phantom values behind.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarEventInput {
    summary: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    location: Option<String>,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    /// True for events the user picked "All day" on. The server stores
    /// these as `VALUE=DATE` ranges; we re-derive that from the start /
    /// end times being a midnight…23:59:59 window.
    #[serde(default)]
    all_day: bool,
    #[serde(default)]
    url: Option<String>,
    /// `OPAQUE` (busy) or `TRANSPARENT` (free). Matches the editor's
    /// "show as" picker. `None` means "leave whatever the server had".
    #[serde(default)]
    transparency: Option<String>,
    #[serde(default)]
    attendees: Vec<EventAttendee>,
    #[serde(default)]
    reminders: Vec<EventReminder>,
    /// `GEO` latitude / longitude (RFC 5545 §3.8.1.6).  Set by the
    /// EventEditor's location-autocomplete pick (#280); `None`
    /// when the user typed the location free-text without
    /// selecting a geocoded match.
    #[serde(default)]
    latitude: Option<f64>,
    #[serde(default)]
    longitude: Option<f64>,
}

/// Build a `CalendarEvent` skeleton from form input. Caller fills in
/// `id` (a fresh UID for create, the cached UID for update). Recurrence
/// fields stay empty here — the editor doesn't expose them yet, and
/// any existing recurrence is preserved from the cached event by the
/// update command before this struct is rebuilt.
fn input_to_calendar_event(uid: &str, input: &CalendarEventInput) -> CalendarEvent {
    // For all-day events the editor sends midnight UTC starts; snap
    // the end to 23:59:59 of the last covered day so `build_ics`
    // recognises the all-day shape. For timed events we trust the
    // editor's exact instants.
    let (start, end) = if input.all_day {
        use chrono::TimeZone;
        let start_day = input.start.date_naive();
        let end_day = input.end.date_naive();
        let s = chrono::Utc.from_utc_datetime(&start_day.and_hms_opt(0, 0, 0).unwrap());
        let e = chrono::Utc.from_utc_datetime(&end_day.and_hms_opt(23, 59, 59).unwrap());
        (s, e)
    } else {
        (input.start, input.end)
    };
    CalendarEvent {
        id: uid.to_string(),
        summary: input.summary.clone(),
        description: input.description.clone(),
        start,
        end,
        location: input.location.clone(),
        rrule: None,
        rdate: vec![],
        exdate: vec![],
        recurrence_id: None,
        url: input.url.clone(),
        transparency: input.transparency.clone(),
        attendees: input.attendees.clone(),
        reminders: input.reminders.clone(),
        latitude: input.latitude,
        longitude: input.longitude,
    }
}

/// Convert a `CalendarEvent` (post-write) into the row shape the cache
/// expects. Used by both `create_calendar_event` and
/// `update_calendar_event` so the local cache reflects the new state
/// without waiting for the next sync round.
fn calendar_event_to_row(
    event: &CalendarEvent,
    href: &str,
    etag: &str,
    ics_raw: &str,
) -> CalendarEventRow {
    CalendarEventRow {
        uid: event.id.clone(),
        recurrence_id: event.recurrence_id,
        href: href.to_string(),
        etag: etag.to_string(),
        summary: event.summary.clone(),
        description: event.description.clone(),
        start: event.start,
        end: event.end,
        location: event.location.clone(),
        rrule: event.rrule.clone(),
        rdate: event.rdate.clone(),
        exdate: event.exdate.clone(),
        url: event.url.clone(),
        transparency: event.transparency.clone(),
        attendees: event.attendees.clone(),
        reminders: event.reminders.clone(),
        latitude: event.latitude,
        longitude: event.longitude,
        ics_raw: ics_raw.to_string(),
    }
}

/// Resolve the `(email, display_name)` to write into `ORGANIZER`
/// for an outbound VEVENT.  This drives whether NC's iMIP plugin
/// can route the invite via the user's real Mail-app SMTP (NC 30+
/// Mail Provider): the address must match the user's primary
/// email exactly, otherwise NC falls back to the system mailer
/// with `From: invitations-noreply@…`.
///
/// Strategy:
/// 1. **When attendees are present**, fetch the user's profile
///    from `/ocs/v2.php/cloud/user`.  Its `email` field is what
///    NC's Mail Provider keys against — same source of truth NC
///    uses internally, so we can't get it wrong.
/// 2. **When the OCS lookup fails or returns no email**, fall
///    back to `organizer_local` (username if it parses as an
///    email, else `username@server-host`) so the PUT still
///    succeeds.  The fallback may not match a Mail-app account,
///    in which case NC's system mailer takes over — better than
///    failing the save.
/// 3. **When there are no attendees**, skip the network call
///    entirely and use the local fallback.  NC's scheduling plugin
///    won't fire without attendees, so `ORGANIZER` here is just
///    metadata for the calendar copy.
async fn resolve_organizer(
    account: &NextcloudAccount,
    app_password: &str,
    has_attendees: bool,
) -> (String, Option<String>) {
    if !has_attendees {
        return organizer_local(account);
    }
    match nimbus_nextcloud::user::fetch_current_user(
        &account.server_url,
        &account.username,
        app_password,
        &account.trusted_certs,
    )
    .await
    {
        Ok(profile) => {
            if let Some(email) = profile.email {
                let name = profile
                    .display_name
                    .or_else(|| account.display_name.clone());
                return (email, name);
            }
            tracing::warn!(
                "Nextcloud user has no email set in Personal info — \
                 iMIP will fall back to system mailer"
            );
        }
        Err(e) => tracing::warn!("OCS user lookup failed, using fallback ORGANIZER: {e}"),
    }
    organizer_local(account)
}

/// Local-only fallback when we can't reach OCS.  Same shape we used
/// before: prefer `username` when it's already an email, else
/// synthesise `username@host`.  This is unrouteable on the public
/// internet but satisfies Sabre's "ATTENDEE without ORGANIZER is
/// 403" check so the PUT itself succeeds.
fn organizer_local(account: &NextcloudAccount) -> (String, Option<String>) {
    let email = if account.username.contains('@') {
        account.username.clone()
    } else {
        let host = account
            .server_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .split('/')
            .next()
            .unwrap_or("nextcloud.local");
        format!("{}@{}", account.username, host)
    };
    (email, account.display_name.clone())
}

/// Create a new VEVENT in the given calendar.
///
/// Generates a fresh UUID for the UID so callers don't have to.
/// `calendars-updated` event payload (#236 follow-up).  Fired when
/// the cache flips a calendar's `read_only` flag — currently the
/// only writer is the CalDAV-write fallback below, but the event
/// is generic so other future flips (e.g. a successful re-sync
/// that rolls a calendar back to writable) can ride the same
/// channel.  The frontend listens, refetches `get_cached_calendars`,
/// and refreshes any `EventEditor` already mounted.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CalendarsUpdatedPayload {
    nextcloud_account_id: Option<String>,
}

/// Inspect a CalDAV-write error for the 403/404 permission signal
/// and, if present, mark the affected calendar as `read_only` in
/// the local cache.  Best-effort: failures here are logged and
/// dropped — the user already saw the upstream write fail; the
/// only loss from a missed flip is that they'd see the same
/// error again on the next attempt.
///
/// Emits `calendars-updated` so the EventEditor (already open
/// with the failed save) refreshes its `calendars` prop and the
/// `currentCalendarReadOnly` derived flips, hiding Save + Delete.
fn flag_calendar_read_only_on_forbidden(
    app: &AppHandle,
    cache: &Cache,
    calendar_id: &str,
    err: &NimbusError,
) {
    if !matches!(err, NimbusError::CalDavWriteForbidden(_)) {
        return;
    }
    if let Err(e) = cache.set_calendar_read_only(calendar_id, true) {
        tracing::warn!(
            "failed to flip read_only=true on calendar '{calendar_id}' after CalDAV 403/404: {e}"
        );
        return;
    }
    tracing::info!(
        "calendar '{calendar_id}' marked read-only locally after CalDAV write was forbidden"
    );
    // Resolve the NC account id from the calendar id (`{nc}::{path}`)
    // so the frontend listener can scope its refresh — costs nothing
    // to include and lets a future multi-account UI avoid blanket
    // refetches.
    let nc_account_id = calendar_id.split_once("::").map(|(nc, _)| nc.to_string());
    let payload = CalendarsUpdatedPayload {
        nextcloud_account_id: nc_account_id,
    };
    if let Err(e) = app.emit("calendars-updated", &payload) {
        tracing::warn!("failed to emit calendars-updated event: {e}");
    }
}

/// The PUT uses `If-None-Match: *`, so a UID collision surfaces as
/// a structured error instead of a silent overwrite. On success, the
/// new event is upserted into the local cache so the UI can render it
/// without waiting for the next sync.
#[tauri::command]
async fn create_calendar_event(
    calendar_id: String,
    input: CalendarEventInput,
    cache: State<'_, Cache>,
    app: AppHandle,
) -> Result<CalendarEvent, NimbusError> {
    let (nc_id, calendar_path) =
        cache
            .get_calendar_server_path(&calendar_id)?
            .ok_or_else(|| {
                NimbusError::Other(format!(
                    "calendar '{calendar_id}' is not in the local cache — refresh and try again"
                ))
            })?;
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    let uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let event = input_to_calendar_event(&uid, &input);
    let (organizer_email, organizer_name) =
        resolve_organizer(&account, &app_password, !event.attendees.is_empty()).await;
    let ics = caldav_build_ics(&event, Some(&organizer_email), organizer_name.as_deref());

    // `calendar_path` from the cache is already an absolute URL —
    // `nimbus-caldav::discovery` resolves it via `absolute_url` before
    // storing. Don't re-prefix the server origin or the PUT goes to
    // `https://hosthttps://host/...`.
    let outcome = caldav_create_event(
        &account.server_url,
        &calendar_path,
        &account.username,
        &app_password,
        &uid,
        &ics,
        &account.trusted_certs,
    )
    .await
    .inspect_err(|e| flag_calendar_read_only_on_forbidden(&app, &cache, &calendar_id, e))?;

    let row = calendar_event_to_row(&event, &outcome.href, &outcome.etag, &ics);
    cache.upsert_single_event(&calendar_id, &row)?;

    // Re-derive the app-side id the same way `event_row_id` does so the
    // returned event matches what `get_cached_events` will surface.
    let mut out = event;
    out.id = format!("{calendar_id}::{uid}");
    Ok(out)
}

/// Update an existing VEVENT, keyed by its app-side id.
///
/// Preserves the cached UID and href; everything else comes from the
/// editor input. The PUT is gated on the cached etag so a concurrent
/// edit on another device surfaces as a structured error (412 → human-
/// readable string) instead of overwriting the other change silently.
#[tauri::command]
async fn update_calendar_event(
    event_id: String,
    input: CalendarEventInput,
    cache: State<'_, Cache>,
    app: AppHandle,
) -> Result<CalendarEvent, NimbusError> {
    let handle = load_event_handle(&cache, &event_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;

    let mut event = input_to_calendar_event(&handle.uid, &input);
    // Preserve recurrence info the editor doesn't surface — losing it
    // would silently demote a recurring series back to a singleton.
    event.recurrence_id = handle.recurrence_id;

    let (organizer_email, organizer_name) =
        resolve_organizer(&account, &app_password, !event.attendees.is_empty()).await;
    let ics = caldav_build_ics(&event, Some(&organizer_email), organizer_name.as_deref());
    // Use the etag-aware retry helper so a concurrent edit on
    // another device (NC web, phone) doesn't surface to the
    // user as "refresh and try again" — it transparently syncs
    // and re-PUTs once.
    let outer_calendar_id = handle.calendar_id.clone();
    let (outcome, handle) = update_event_with_etag_retry(&cache, &event_id, &ics)
        .await
        .inspect_err(|e| {
            flag_calendar_read_only_on_forbidden(&app, &cache, &outer_calendar_id, e)
        })?;

    let row = calendar_event_to_row(&event, &outcome.href, &outcome.etag, &ics);
    cache.upsert_single_event(&handle.calendar_id, &row)?;

    let mut out = event;
    out.id = event_id;
    Ok(out)
}

/// Delete an event from the server and the local cache.  Server-side
/// iTIP CANCEL notices to attendees are emitted by Nextcloud's
/// `OCA\DAV\CalDAV\Schedule\IMipPlugin` on the DELETE — no
/// client-side mail involved.
#[tauri::command]
async fn delete_calendar_event(
    event_id: String,
    cache: State<'_, Cache>,
    app: AppHandle,
) -> Result<(), NimbusError> {
    let handle = load_event_handle(&cache, &event_id)?;
    let nc_account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;

    let calendar_id = handle.calendar_id.clone();
    caldav_delete_event(
        &handle.href,
        &nc_account.username,
        &app_password,
        &handle.etag,
        &nc_account.trusted_certs,
    )
    .await
    .inspect_err(|e| flag_calendar_read_only_on_forbidden(&app, &cache, &calendar_id, e))?;
    cache.delete_event_by_id(&event_id)?;
    Ok(())
}

// ── Scheduling-assistant availability (#137) ─────────────────
//
// `get_attendee_availability` powers the EventPlanner UI: given a
// list of attendee email addresses and a time window, return each
// person's busy slots so the UI can render a free/busy grid.
//
// Resolution order per attendee:
//
//   1. **Sharees lookup** — does this address belong to a local NC
//      user?  If yes, run a CalDAV `free-busy-query` REPORT against
//      their calendar home.  Returns busy periods only (no event
//      details), so the privacy story is identical to the standard
//      Nextcloud / Outlook free-busy lookup users already expect.
//   2. **Free-busy succeeded** → emit them with `source =
//      "nc-freebusy"`.  This is the authoritative signal.
//   3. **Free-busy failed** (server refused, calendar not shared,
//      network blip) → fall through to the local-cache scan.
//   4. **Not an NC user, or NC lookup failed** → scan our own
//      calendars for events where this address is listed as an
//      attendee.  Surfaces the meetings *we* know about that the
//      person was invited to.  Issued via `source = "local-cache"`.
//   5. **Anything else** → empty list with `source = "unknown"`.
//      The UI renders the row as "no signal — assume free".
//
// The local-cache scan piggybacks on the existing recurrence-
// expanded `expand_calendar_events_in_range` so a series the
// attendee was invited to surfaces every occurrence in the window.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AttendeeBusyPeriod {
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    /// One of "busy", "tentative", "unavailable", "free".  The
    /// planner UI maps these to its own colour palette.
    kind: String,
    /// Source event's summary, when the period came from our
    /// local-cache scan (the user's own calendars where the
    /// attendee is listed).  CalDAV free-busy responses
    /// deliberately don't carry titles — privacy — so this
    /// stays `None` for `nc-freebusy` periods.  Surfacing it
    /// in the planner is fine because the user already owns
    /// the event whose title we're showing.
    summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AttendeeAvailability {
    email: String,
    display_name: Option<String>,
    /// "nc-freebusy" | "local-cache" | "unknown" — see resolution
    /// order in the module-level comment above.
    source: String,
    busy_periods: Vec<AttendeeBusyPeriod>,
}

fn busy_kind_to_string(k: CaldavBusyKind) -> String {
    match k {
        CaldavBusyKind::Busy => "busy",
        CaldavBusyKind::Tentative => "tentative",
        CaldavBusyKind::Unavailable => "unavailable",
        CaldavBusyKind::Free => "free",
    }
    .to_string()
}

#[tauri::command]
async fn get_attendee_availability(
    nc_id: String,
    attendee_emails: Vec<String>,
    range_start: chrono::DateTime<chrono::Utc>,
    range_end: chrono::DateTime<chrono::Utc>,
    cache: State<'_, Cache>,
) -> Result<Vec<AttendeeAvailability>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    // Pre-load the local-cache events once so the per-attendee
    // scan loop doesn't repeat the SQL + expansion work.
    let calendar_ids: Vec<String> = cache
        .list_calendars(&nc_id)?
        .into_iter()
        .map(|c| c.id)
        .collect();
    let local_events =
        expand_calendar_events_in_range(&cache, &calendar_ids, range_start, range_end)?;

    let mut out: Vec<AttendeeAvailability> = Vec::with_capacity(attendee_emails.len());

    for email in attendee_emails {
        let lower = email.trim().to_ascii_lowercase();
        if lower.is_empty() {
            continue;
        }

        // Step 1: sharees lookup.  Soft-fail (None) on errors so a
        // single bad lookup doesn't blank out the planner.
        let nc_match = match nimbus_nextcloud::find_user_by_email(
            &account.server_url,
            &account.username,
            &app_password,
            &email,
            &account.trusted_certs,
        )
        .await
        {
            Ok(m) => m,
            Err(e) => {
                tracing::info!("sharees lookup for '{email}' failed: {e}");
                None
            }
        };

        // Always pre-compute the local-cache hits — events from
        // the user's own calendars (which include shared/subscribed
        // calendars in NC) where this person is listed.  Used both
        // as the fallback when free-busy fails AND as a title
        // source to enrich free-busy periods that come back without
        // names attached.
        let local_for_attendee: Vec<(
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
            Option<String>,
        )> = local_events
            .iter()
            .filter(|ev| {
                ev.attendees
                    .iter()
                    .any(|a| a.email.to_ascii_lowercase() == lower)
            })
            .map(|ev| {
                (
                    ev.start,
                    ev.end,
                    if ev.summary.trim().is_empty() {
                        None
                    } else {
                        Some(ev.summary.clone())
                    },
                )
            })
            .collect();

        // Step 2: NC user → free-busy-query.
        if let Some(m) = nc_match.as_ref() {
            let principal_url = caldav_nc_principal_home(&account.server_url, &m.user_id);
            match caldav_query_free_busy(
                &principal_url,
                &account.username,
                &app_password,
                range_start,
                range_end,
                &account.trusted_certs,
            )
            .await
            {
                Ok(periods) => {
                    out.push(AttendeeAvailability {
                        email,
                        display_name: Some(m.display_name.clone()),
                        source: "nc-freebusy".into(),
                        busy_periods: periods
                            .into_iter()
                            .map(|p| AttendeeBusyPeriod {
                                start: p.start,
                                end: p.end,
                                kind: busy_kind_to_string(p.kind),
                                // Free-busy responses themselves
                                // don't carry titles by design,
                                // but if we *also* have the same
                                // event in our local cache (the
                                // attendee invited us, or their
                                // calendar is shared with us),
                                // surface its title — that's not
                                // a privacy regression because we
                                // already own the data on our
                                // side.  Match on start time;
                                // server-side regeneration of
                                // free-busy uses the source
                                // event's DTSTART verbatim.
                                summary: local_for_attendee
                                    .iter()
                                    .find(|(s, _, _)| *s == p.start)
                                    .and_then(|(_, _, sum)| sum.clone()),
                            })
                            .collect(),
                    });
                    continue;
                }
                Err(e) => {
                    // Common case: the calendar isn't shared with
                    // us, so the REPORT 403/404s.  Drop down to the
                    // local-cache scan.
                    tracing::info!(
                        "free-busy-query unavailable for {} ({email}): {e}",
                        m.user_id
                    );
                }
            }
        }

        // Step 3: local-cache fallback — events in the user's own
        // calendars where this person is listed as an attendee.
        let busy: Vec<AttendeeBusyPeriod> = local_for_attendee
            .iter()
            .map(|(start, end, summary)| AttendeeBusyPeriod {
                start: *start,
                end: *end,
                kind: "busy".into(),
                summary: summary.clone(),
            })
            .collect();

        let display_name = nc_match.as_ref().map(|m| m.display_name.clone());
        let source = if !busy.is_empty() {
            "local-cache"
        } else if nc_match.is_some() {
            // We knew it was an NC user but free-busy failed and
            // we have no local events for them — leave the row
            // empty with `unknown` so the UI distinguishes "no
            // signal" from "confirmed free".
            "unknown"
        } else {
            "local-cache"
        }
        .to_string();

        out.push(AttendeeAvailability {
            email,
            display_name,
            source,
            busy_periods: busy,
        });
    }

    Ok(out)
}

// ── Location autocomplete + map preview (#280) ───────────────
//
// The EventEditor's Location field offers two affordances:
//
//   1. **Autocomplete** — keystrokes (debounced) call
//      `geocode_search`, which dedupes against the local
//      `geocode_cache` table before hitting Nominatim.  Picking
//      a suggestion stamps the canonical `display_name` plus
//      `(lat, lon)` onto the in-flight event, which then
//      round-trips through `LOCATION` + `GEO` in the iCalendar
//      body.
//
//   2. **Inline map preview** — once the event has a `(lat,
//      lon)`, the UI mounts a small MapLibre canvas pointing at
//      it.  All tile traffic goes to public OSM-backed tile
//      services with attribution (see the frontend component).
//
// `detect_nc_maps` is informational: it tells the UI whether
// the user's connected NC has the Maps app enabled so the UI
// can surface "Using your Nextcloud Maps" in the autocomplete
// header.  The actual geocoding still goes to Nominatim either
// way — NC Maps doesn't expose a server-side proxy at present.

#[tauri::command]
async fn geocode_search(
    query: String,
    lang: Option<String>,
    cache: State<'_, Cache>,
) -> Result<Vec<geocode::GeocodeResult>, NimbusError> {
    let lang = lang.unwrap_or_default();
    // Cache hit short-circuits the network round-trip.  The
    // cache itself canonicalises the query (whitespace,
    // case-folding) so a tiny stylistic typo doesn't burn an
    // upstream call.
    if let Some(json) = cache
        .get_geocode_cache(&query, &lang)
        .map_err(NimbusError::from)?
    {
        if let Ok(hits) = serde_json::from_str::<Vec<geocode::GeocodeResult>>(&json) {
            return Ok(hits);
        }
        // Cache row exists but is corrupt — fall through to a
        // fresh fetch and let the new payload overwrite it.
        tracing::warn!("geocode_cache: corrupt row for {query:?}, refetching");
    }

    let hits = geocode::nominatim_search(&query, &lang).await?;
    let serialised = serde_json::to_string(&hits)
        .map_err(|e| NimbusError::Other(format!("geocode result serialise: {e}")))?;
    if let Err(e) = cache.put_geocode_cache(&query, &lang, &serialised) {
        // Cache write failure is non-fatal — the user still
        // gets the live result.
        tracing::warn!("geocode_cache write failed: {e}");
    }
    Ok(hits)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NextcloudMapsCapability {
    /// True when the connected NC has the Maps app enabled.
    available: bool,
}

#[tauri::command]
async fn detect_nc_maps(nc_id: String) -> Result<NextcloudMapsCapability, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    // The capabilities OCS endpoint returns an enabled-apps map
    // we can scan for `maps` without needing to actually call
    // any Maps-app endpoints.  Soft-fails to "not available"
    // on any network blip — the UI just shows the generic
    // OSM-attribution copy in that case.
    let server = account.server_url.trim_end_matches('/');
    let url = format!("{server}/ocs/v2.php/cloud/capabilities?format=json");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| NimbusError::Network(format!("capabilities client: {e}")))?;
    let resp = client
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(&account.username, Some(&app_password))
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("capabilities request: {e}")))?;
    if !resp.status().is_success() {
        return Ok(NextcloudMapsCapability { available: false });
    }
    // The capabilities body is deeply nested; we just look for
    // any case-insensitive hint of "maps" inside the
    // capabilities key.  More precise parsing would tie us to
    // an NC version's exact JSON shape — this is informational
    // anyway, and a false negative just means the UI doesn't
    // show the "via NC Maps" hint.
    let body = resp.text().await.unwrap_or_default();
    let available = body.to_ascii_lowercase().contains("\"maps\"");
    Ok(NextcloudMapsCapability { available })
}

/// Remove a locally-cached event whose iCalendar `UID` matches
/// `uid`.  Surfaced from the inbound CANCEL card in MailView:
/// when an external organiser sends a `METHOD:CANCEL` mail, the
/// user clicks "Remove from my calendar" and we DELETE the
/// CalDAV resource so the cancelled meeting disappears from the
/// grid (and from any other CalDAV client, including their
/// phone).  Idempotent: returns `Ok(())` when no row matches —
/// the user may have already removed the event manually, or the
/// invite never made it into their calendar in the first place.
///
/// Note that we don't fight Sabre's iTIP machinery here.  An
/// attendee-side DELETE of an event whose ORGANIZER is external
/// would normally generate a `METHOD:REPLY;PARTSTAT=DECLINED`
/// from NC's IMipPlugin; that's not what we want when responding
/// to a CANCEL (the organiser already cancelled — a "decline" is
/// noise).  In practice Sabre suppresses REPLY emission when the
/// stored event already carries `STATUS:CANCELLED` or the user's
/// PARTSTAT is unchanged from the previous version, which covers
/// the common case.  Worth flagging explicitly if it turns out
/// to send spurious mail in the wild.
/// True when an event with the given iCalendar UID exists in
/// any of the user's locally-cached calendars.  Used by the
/// CANCEL card to decide whether to expose "Remove from my
/// calendar" — only makes sense when there's actually a local
/// copy to remove.  A miss here is the common case for invites
/// the user never accepted (CANCEL arrives but the event was
/// never imported into a calendar): the card should fall back
/// to a passive "not in your calendar" line instead of the
/// remove button.
#[tauri::command]
fn is_event_in_calendar(uid: String, cache: State<'_, Cache>) -> Result<bool, NimbusError> {
    Ok(cache.find_event_id_by_uid(&uid)?.is_some())
}

/// Record that an iCalendar UID has been cancelled by its
/// organiser.  Called by MailView when it surfaces a
/// `METHOD:CANCEL` mail, so the original REQUEST mail's RSVP
/// card can flip to the cancelled flavour on its next open.
#[tauri::command]
fn record_cancelled_invite(uid: String, cache: State<'_, Cache>) -> Result<(), NimbusError> {
    cache.mark_invite_cancelled(&uid).map_err(NimbusError::from)
}

/// True when MailView has previously observed a `METHOD:CANCEL`
/// mail for this iCalendar UID.  Used by the RSVP card to
/// flip the original REQUEST mail's flavour to the cancelled
/// banner so the user doesn't unwittingly answer a meeting
/// that's been cancelled.
#[tauri::command]
fn is_invite_cancelled(uid: String, cache: State<'_, Cache>) -> Result<bool, NimbusError> {
    cache.is_invite_cancelled(&uid).map_err(NimbusError::from)
}

#[tauri::command]
async fn dismiss_cancelled_event(uid: String, cache: State<'_, Cache>) -> Result<(), NimbusError> {
    let Some(event_id) = cache.find_event_id_by_uid(&uid)? else {
        tracing::info!(
            "dismiss_cancelled_event: no cached event with UID {uid}, treating as no-op"
        );
        return Ok(());
    };
    let handle = load_event_handle(&cache, &event_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;
    // Use the silent variant — without `Schedule-Reply: F`,
    // Sabre's attendee-side DELETE handler emits a spurious
    // `METHOD:REPLY;PARTSTAT=DECLINED` to the organiser.  The
    // organiser already sent CANCEL; mailing them a decline
    // back is just noise (and confusing).
    nimbus_caldav::delete_event_silent(
        &handle.href,
        &account.username,
        &app_password,
        &handle.etag,
        &account.trusted_certs,
    )
    .await?;
    cache.delete_event_by_id(&event_id)?;
    Ok(())
}

// ── iTIP / iMIP (#58) ─────────────────────────────────────────────
//
// Outbound: when Compose's "Add Event" flow saves an event, we hand
// the recipient mail clients a `text/calendar; method=REQUEST`
// attachment so any RFC-compliant client can save the invite
// natively.
//
// Inbound: when a received message carries a `text/calendar` part,
// we parse the iCalendar and surface an "invite card" with
// Accept / Decline / Tentative buttons.  Each click silently
// emits a `text/calendar; method=REPLY` email back to the
// organiser — that's the iMIP RSVP loop (RFC 6047).

/// Lightweight iCalendar summary the JS layer renders for an
/// inbound invite (Accept / Decline / Tentative card).  Picks
/// the smallest set of fields the card needs; the full ICS bytes
/// stay on the Rust side and ride through `send_event_rsvp` so
/// the REPLY can carry the same UID and DTSTAMP without the
/// frontend having to round-trip the full event.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InviteSummary {
    /// Calendar-level `METHOD:` value, upper-cased.  iTIP defines
    /// REQUEST (organiser → attendee), REPLY (attendee →
    /// organiser), CANCEL, PUBLISH, REFRESH, COUNTER, DECLINECOUNTER.
    /// `MailView` only shows the RSVP card for REQUEST; the others
    /// (especially REPLY) are typically attendee responses to OUR
    /// invites and don't need a "you can RSVP" card on the
    /// organiser's side.  `None` means no METHOD line was present
    /// (treat as "not an iTIP message" and suppress the card).
    method: Option<String>,
    /// VEVENT UID — the join key between REQUEST + REPLY.
    uid: String,
    /// SUMMARY (title) of the event.
    summary: String,
    /// DTSTART, normalised to UTC (RFC 3339).
    start: chrono::DateTime<chrono::Utc>,
    /// DTEND, normalised to UTC.
    end: chrono::DateTime<chrono::Utc>,
    /// Optional venue / room.
    location: Option<String>,
    /// Optional URL — Talk join links, video conferencing, etc.
    url: Option<String>,
    /// ORGANIZER's email (mailto: URI stripped).  Required by RFC
    /// 5546 whenever any ATTENDEE is present, so we expect it on
    /// real-world invites — but a missing one isn't fatal, the
    /// RSVP just falls back to the message's From: address.
    organizer_email: Option<String>,
    organizer_name: Option<String>,
    /// All ATTENDEEs from the VEVENT.  The card highlights the
    /// row matching the current user's address so they can see
    /// their own NEEDS-ACTION status before clicking.
    attendees: Vec<nimbus_core::models::EventAttendee>,
    /// The full ICS body, used to preserve UID + DTSTAMP +
    /// SEQUENCE on the REPLY without re-fetching.
    raw_ics: String,
}

/// Parse a raw `text/calendar` byte slice into the slim
/// `InviteSummary` the inbound RSVP card consumes.  Looks at the
/// FIRST VEVENT in the file — recurring series and overrides are
/// out of scope for the invite card MVP (the user can still
/// manage them in the Calendar view after accepting).
///
/// `parse_ics` doesn't surface ORGANIZER as a typed field today,
/// so the JS caller is expected to fall back to the message's
/// `From:` header for the recipient of the RSVP REPLY — which is
/// what RFC 5546 says the organiser address tracks anyway.
#[tauri::command]
fn parse_event_invite(bytes: Vec<u8>) -> Result<InviteSummary, NimbusError> {
    let body = String::from_utf8(bytes)
        .map_err(|e| NimbusError::Protocol(format!("invite is not UTF-8: {e}")))?;
    let events = nimbus_caldav::ical::parse_ics(&body)
        .map_err(|e| NimbusError::Protocol(format!("could not parse calendar invite: {e}")))?;
    let event = events
        .into_iter()
        .next()
        .ok_or_else(|| NimbusError::Protocol("invite contains no VEVENT".into()))?;

    let method = extract_calendar_method(&body);

    Ok(InviteSummary {
        method,
        uid: event.id.clone(),
        summary: event.summary.clone(),
        start: event.start,
        end: event.end,
        location: event.location.clone(),
        url: event.url.clone(),
        organizer_email: None,
        organizer_name: None,
        attendees: event.attendees.clone(),
        raw_ics: body,
    })
}

/// Pull the calendar-level `METHOD:` value out of a raw ICS body
/// without round-tripping through a full parser.  iTIP defines
/// the line as a single token after the colon (REQUEST / REPLY /
/// CANCEL / etc.); we just normalise to upper case so JS-side
/// equality checks don't have to be case-insensitive.
fn extract_calendar_method(ics: &str) -> Option<String> {
    for line in ics.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("METHOD:") {
            let m = rest.trim();
            if !m.is_empty() {
                return Some(m.to_uppercase());
            }
        }
    }
    None
}

/// Generate a `METHOD:REPLY` iCalendar body for the user's RSVP
/// response.  Re-renders the original event with PARTSTAT updated
/// for the current user's ATTENDEE row only — every other
/// ATTENDEE keeps whatever the inbound message had so the
/// organiser's mail client doesn't see spurious "everyone
/// changed" diffs.
///
/// Respond to an inbound invite by writing the user's PARTSTAT to
/// CalDAV.  Nextcloud's CalDAV-Schedule plugin (with NC 30+ Mail
/// Provider) generates and SMTPs the iMIP REPLY automatically —
/// the client never touches SMTP for RSVPs.
///
/// Behaviour by partstat:
/// - **ACCEPTED**: PUT into `calendar_id` with PARTSTAT=ACCEPTED,
///   TRANSP=OPAQUE.  The event lands on the user's calendar (and
///   syncs to their phone), and NC mails the organiser.
/// - **TENTATIVE**: PUT with PARTSTAT=TENTATIVE, TRANSP=TRANSPARENT
///   so the calendar can render it visually distinct (striped
///   pattern in CalendarView).
/// - **DECLINED**: PUT with PARTSTAT=DECLINED, then DELETE the
///   resource.  The PUT triggers NC's REPLY (organiser notified);
///   the DELETE removes the entry from the user's calendar so
///   declined meetings don't clutter the grid.
///
/// Resolving the responding attendee's address goes through
/// **every identity Nimbus knows about**, not just one: the NC
/// user-profile email (Sabre's principal CUA), every configured
/// mail-account address, plus an optional `attendee_email`
/// hint from the card (the address the inbound mail was
/// actually sent to).  We intersect that combined set with the
/// inbound ATTENDEE list and use whichever address is *already
/// in the invite* — that's the row Sabre's iTIP broker will
/// match on the user's principal-CUA when generating the
/// REPLY iMIP.
///
/// Why so many sources?  The chain is fragile: NC profile
/// email → Sabre principal CUA → ATTENDEE-row match →
/// IMipPlugin Mail Provider lookup against Mail-app accounts.
/// All four addresses must equal each other for REPLY mail to
/// actually leave NC.  Pinning to a single source means a
/// single misconfiguration (empty NC profile email, mismatched
/// Mail-app primary, etc.) silently breaks REPLY delivery —
/// exactly what was happening before.
// `attendee_hint`: optional hint from the card — the address
// the inbound mail was actually sent to, resolved by the
// frontend from the invite's ATTENDEE list intersected with
// the user's configured mail-account addresses.  Used as the
// highest-priority candidate when picking the row to mutate +
// identify with on Sabre's principal CUA.  May be `None` if
// the card couldn't resolve one.
#[tauri::command]
async fn respond_to_invite(
    calendar_id: String,
    raw_ics: String,
    partstat: String,
    attendee_hint: Option<String>,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    // Resolve the chosen calendar's location on the server.
    let (nc_id, calendar_path) =
        cache
            .get_calendar_server_path(&calendar_id)?
            .ok_or_else(|| {
                NimbusError::Other(format!(
                    "calendar '{calendar_id}' is not in the local cache — refresh and try again"
                ))
            })?;
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    // Build the candidate-identity list, in priority order:
    //   1. The card's hint (transport-derived, most likely
    //      verbatim in the invite).
    //   2. NC profile email — Sabre's principal CUA, the
    //      authoritative identity for the iTIP broker.
    //   3. Every configured mail-account address (covers the
    //      "I added a Nimbus mail account whose email differs
    //      from my NC profile" case).
    //   4. The synth `username@server-host` as a last resort.
    // We then take the FIRST candidate that actually appears
    // in the inbound ATTENDEE list — Sabre will match the
    // same row when scanning the body for the principal's CUA.
    // If no candidate matches, we fall back to candidate #2
    // (NC profile email — the address Sabre's broker is most
    // likely to identify as ours) and add a fresh row, so the
    // server-side iTIP can still pair us against the principal.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(hint) = attendee_hint.as_deref() {
        let h = hint.trim();
        if !h.is_empty() {
            candidates.push(h.to_string());
        }
    }
    let nc_profile_email = match nimbus_nextcloud::user::fetch_current_user(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await
    {
        Ok(p) => p.email,
        Err(e) => {
            tracing::warn!("RSVP: NC user-profile lookup failed ({e})");
            None
        }
    };
    if let Some(e) = nc_profile_email.as_deref() {
        candidates.push(e.to_string());
    }
    if let Ok(mail_accounts) = account_store::load_accounts(&cache) {
        for a in mail_accounts {
            candidates.push(a.email);
        }
    }
    candidates.push(organizer_local(&account).0);
    // Lower-cased, deduplicated, preserving priority order.
    let mut seen = std::collections::HashSet::new();
    let candidates: Vec<String> = candidates
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .filter(|s| seen.insert(s.to_ascii_lowercase()))
        .collect();
    tracing::debug!("RSVP candidate identities: {candidates:?}");

    // Pick the first candidate already present in the inbound
    // ATTENDEE list.  If none match, default to the NC profile
    // email (so Sabre's broker matches the new row we'll add
    // against its principal CUA) — and last-ditch the first
    // non-empty candidate so we always have something.
    let attendee_email = {
        let inbound_attendees: Vec<String> = nimbus_caldav::ical::parse_ics(&raw_ics)
            .ok()
            .and_then(|v| v.into_iter().next())
            .map(|e| e.attendees.into_iter().map(|a| a.email).collect())
            .unwrap_or_default();
        let inbound_set: std::collections::HashSet<String> = inbound_attendees
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        candidates
            .iter()
            .find(|c| inbound_set.contains(&c.to_ascii_lowercase()))
            .cloned()
            .or(nc_profile_email)
            .or_else(|| candidates.into_iter().next())
            .unwrap_or_else(|| organizer_local(&account).0)
    };
    tracing::info!("RSVP: using attendee identity {attendee_email}");

    // Parse the inbound ICS, flip the matching attendee's PARTSTAT,
    // and (for TENTATIVE) override TRANSP so the calendar renders
    // it differently.
    let events = nimbus_caldav::ical::parse_ics(&raw_ics)
        .map_err(|e| NimbusError::Protocol(format!("could not parse invite: {e}")))?;
    let mut event = events
        .into_iter()
        .next()
        .ok_or_else(|| NimbusError::Protocol("invite has no VEVENT".into()))?;

    // Flip the matching ATTENDEE's PARTSTAT.  When no row
    // matches — common for aliases, forwarded invites, or any
    // case where the user's mail-account address differs from
    // what the organiser typed into ATTENDEE — we ADD a fresh
    // row with the user's address instead of failing.  Sabre's
    // iTIP broker keys "is this PUT an RSVP from this user?"
    // off the principal-email match against the ATTENDEE list,
    // and an inserted row satisfies that check exactly the same
    // as a mutated one.  A REPLY then goes out from NC's iMIP
    // plugin with PARTSTAT carrying the user's chosen response.
    let mut matched = false;
    for att in event.attendees.iter_mut() {
        if att.email.eq_ignore_ascii_case(attendee_email.trim()) {
            att.status = Some(partstat.clone());
            // Force iMIP dispatch on the responding row — see
            // EventAttendee::force_send_reply.  Without this,
            // Sabre may process the PARTSTAT change locally
            // but skip the outbound iMIP to the organiser if
            // its "should this notify?" heuristics decline.
            att.force_send_reply = true;
            matched = true;
        }
    }
    if !matched {
        tracing::info!(
            "RSVP for {attendee_email}: address not in original ATTENDEE list, \
             adding a new row with PARTSTAT={partstat}"
        );
        event.attendees.push(EventAttendee {
            email: attendee_email.trim().to_string(),
            common_name: None,
            status: Some(partstat.clone()),
            role: Some("REQ-PARTICIPANT".into()),
            force_send_reply: true,
        });
    }
    if partstat == "TENTATIVE" {
        event.transparency = Some("TRANSPARENT".into());
    } else {
        // ACCEPTED + DECLINED => OPAQUE so the slot blocks (or
        // would block, before the DECLINE-side DELETE wipes it).
        event.transparency = Some("OPAQUE".into());
    }

    // PUT strategy — Sabre's CalDAV-Schedule plugin only fires a
    // REPLY iMIP when it sees a PARTSTAT diff against the
    // previously-stored copy.  A fresh PUT with `If-None-Match: *`
    // creates the resource for the first time and Sabre treats it
    // as the *organiser* writing into their own calendar — no
    // REPLY emerges.  To force the broker to see a real change,
    // first-time PUTs go in two steps:
    //   1. CREATE with the user's row at PARTSTAT=NEEDS-ACTION
    //      (the same state the inbound REQUEST has).  No iTIP
    //      runs here — there's no diff to compare.
    //   2. UPDATE the same href with the user's chosen PARTSTAT.
    //      Sabre sees NEEDS-ACTION → ACCEPTED/TENTATIVE/DECLINED,
    //      generates a METHOD:REPLY iMIP, and IMipPlugin SMTPs it
    //      to ORGANIZER through the system mailer.
    // For events already in the user's cache (re-RSVP / changing
    // your mind), one update_event keyed on the cached etag is
    // enough — Sabre still sees the prior PARTSTAT and emits the
    // REPLY iMIP.

    // The local cache can fall out of sync with the server in
    // ways that matter here: a previous DECLINED RSVP runs PUT
    // followed by DELETE, and Sabre may "soft-delete" by
    // converting the DELETE into a PARTSTAT=DECLINED on the
    // existing resource (so the organiser still sees who
    // declined).  We dropped the local row, but the server still
    // has the resource — so when the user changes their mind,
    // `find_event_id_by_uid` returns None and we'd try to CREATE
    // a fresh resource with the same UID, which the server
    // rejects with 412 ("already exists").  Refresh the cache
    // via a single-calendar CalDAV sync first, so a soft-delete
    // bounces back into the cache and we route through the
    // update path.
    let mut existing_id = cache.find_event_id_by_uid(&event.id)?;
    if existing_id.is_none() {
        if let Err(e) = refresh_calendar_cache(&cache, &nc_id, &calendar_path).await {
            tracing::warn!("RSVP: pre-PUT cache refresh failed (continuing): {e}");
        }
        existing_id = cache.find_event_id_by_uid(&event.id)?;
    }
    // Track the body we actually PUT — used to mirror into the
    // cache afterwards, so the next surgical edit operates on
    // the body that's really on the server (not a regenerated
    // approximation).
    let body_put: String;
    let put_outcome = match existing_id {
        Some(existing_id) => {
            // Surgical-edit path.  Sabre's iTIP broker only
            // dispatches REPLY iMIP when the diff between the
            // stored body and the new PUT is "clean" — just the
            // user's PARTSTAT.  Regenerating the body via
            // `build_ics` drops X-* properties / re-orders /
            // loses params and Sabre then accepts the PARTSTAT
            // change but suppresses the iTIP REPLY (the same
            // restriction NC's web UI works around by editing
            // only the one line).  We do the same here: pull
            // the cached body, surgically replace just the user's
            // ATTENDEE PARTSTAT (and add SCHEDULE-FORCE-SEND=
            // REPLY), preserve everything else byte-for-byte.
            let handle = load_event_handle(&cache, &existing_id)?;
            let surgical = nimbus_caldav::ical::surgical_set_partstat(
                &handle.ics_raw,
                &attendee_email,
                &partstat,
                true,
            );
            let (out, _) = update_event_with_etag_retry(&cache, &existing_id, &surgical).await?;
            body_put = surgical;
            out
        }
        None => {
            // Step 1 with surgical edit on the inbound ICS so
            // the body Sabre stores as the "before" state is a
            // minimal mutation of the original — Sabre's iTIP
            // restrictions accept it cleanly.
            let step1_body = nimbus_caldav::ical::surgical_set_partstat(
                &raw_ics,
                &attendee_email,
                "NEEDS-ACTION",
                false,
            );
            let first = caldav_create_event(
                &account.server_url,
                &calendar_path,
                &account.username,
                &app_password,
                &event.id,
                &step1_body,
                &account.trusted_certs,
            )
            .await?;

            // Step 2 — update keyed on the etag we just got, with
            // the user's chosen PARTSTAT + SCHEDULE-FORCE-SEND.
            // Sabre sees a clean PARTSTAT-only diff against
            // step 1's stored body and dispatches the REPLY iMIP.
            let step2_body = nimbus_caldav::ical::surgical_set_partstat(
                &raw_ics,
                &attendee_email,
                &partstat,
                true,
            );
            let out = caldav_update_event(
                &first.href,
                &account.username,
                &app_password,
                &first.etag,
                &step2_body,
                &account.trusted_certs,
            )
            .await?;
            body_put = step2_body;
            out
        }
    };

    // Mirror the new state into the local cache so CalendarView
    // shows the accepted/tentative event without waiting for the
    // next sync — and so the *next* surgical edit operates on
    // the body that's actually on the server.
    let row = calendar_event_to_row(&event, &put_outcome.href, &put_outcome.etag, &body_put);
    cache.upsert_single_event(&calendar_id, &row)?;

    // DECLINED used to also DELETE the resource here ("no
    // clutter").  That removed user-declined events from the
    // calendar entirely, which made the badge afterwards look
    // like a cancellation (the event wasn't on any calendar but
    // we had a persisted RSVP for it).  Apple Calendar's
    // approach is right: keep the declined event around with
    // PARTSTAT=DECLINED so it stays visible (faded /
    // struck-through in the grid).  CalendarView can render the
    // declined visual state separately; this command just stops
    // deleting the row.

    // Persist the chosen PARTSTAT keyed by UID so the inbox card
    // re-renders the right state on reopen.  This mirrors what's
    // now on the server but avoids a CalDAV round-trip just for
    // UI feedback.
    if let Err(e) = cache.upsert_rsvp_response(&event.id, &partstat) {
        tracing::warn!("failed to persist RSVP response for {}: {e}", event.id);
    }
    Ok(())
}

/// Look up the user's last RSVP answer (ACCEPTED / DECLINED /
/// TENTATIVE) for an iCalendar UID. The invite card calls this on
/// mount so a previously answered invite re-renders in its
/// post-reply state instead of showing fresh Accept/Decline buttons.
#[tauri::command]
async fn get_rsvp_response(
    uid: String,
    cache: State<'_, Cache>,
) -> Result<Option<String>, NimbusError> {
    cache.get_rsvp_response(&uid).map_err(NimbusError::from)
}

/// Read the responding-user's PARTSTAT off the cached calendar
/// event with `uid`, if any.  Source of truth for the inbox
/// RSVP card so it reflects PARTSTAT changes made via NC web
/// UI / the user's phone / any other CalDAV client — not just
/// the changes Nimbus made itself (which is what the local
/// `rsvp_responses` table tracks).
///
/// Runs a **differential CalDAV sync** of the calendar that
/// contains the event before reading, so the card always
/// reflects the latest server state without requiring the user
/// to wait for the background-sync interval.  CalDAV's
/// sync-collection report is incremental (only the deltas since
/// the last sync token), so the round-trip is cheap even on
/// large calendars.
///
/// Identity matching uses the same candidate list
/// `respond_to_invite` builds: the optional `attendee_hint`
/// from the card, the NC profile email, every configured mail
/// account.  Returns `None` when no row matches (or the event
/// isn't in the cache).
#[tauri::command]
async fn get_event_partstat_for_user(
    uid: String,
    attendee_hint: Option<String>,
    cache: State<'_, Cache>,
) -> Result<Option<String>, NimbusError> {
    let Some(event_id) = cache.find_event_id_by_uid(&uid)? else {
        return Ok(None);
    };
    let handle = cache
        .get_event_server_handle(&event_id)?
        .ok_or_else(|| NimbusError::Other("stale calendar cache entry".into()))?;

    // Differential CalDAV sync of the parent calendar — picks
    // up PARTSTAT changes made via NC web UI / phone / any other
    // CalDAV client without waiting for the background-sync
    // interval.  Best-effort: a sync failure leaves the cache
    // as-is and we return the locally-known state.
    if let Some((_, cal_path)) = cache.get_calendar_server_path(&handle.calendar_id)?
        && let Err(e) =
            refresh_calendar_cache(&cache, &handle.nextcloud_account_id, &cal_path).await
    {
        tracing::warn!(
            "RSVP badge: pre-read calendar sync failed (continuing with stale cache): {e}"
        );
    }
    let Some(handle) = cache.get_event_server_handle(&event_id)? else {
        return Ok(None);
    };

    // Build the candidate list — same shape as respond_to_invite.
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;
    let mut candidates: Vec<String> = Vec::new();
    if let Some(h) = attendee_hint.as_deref() {
        let h = h.trim();
        if !h.is_empty() {
            candidates.push(h.to_string());
        }
    }
    if let Ok(profile) = nimbus_nextcloud::user::fetch_current_user(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await
        && let Some(email) = profile.email
    {
        candidates.push(email);
    }
    if let Ok(mail_accounts) = account_store::load_accounts(&cache) {
        for a in mail_accounts {
            candidates.push(a.email);
        }
    }
    let candidates_lc: Vec<String> = candidates.iter().map(|s| s.to_ascii_lowercase()).collect();

    let events = nimbus_caldav::ical::parse_ics(&handle.ics_raw)
        .map_err(|e| NimbusError::Protocol(format!("parse cached event: {e}")))?;
    let partstat = events.into_iter().next().and_then(|event| {
        event.attendees.into_iter().find_map(|att| {
            if candidates_lc.contains(&att.email.to_ascii_lowercase()) {
                att.status.map(|s| s.to_ascii_uppercase())
            } else {
                None
            }
        })
    });
    Ok(partstat)
}

/// `caldav_update_event` with transparent etag-mismatch
/// recovery.  When the cached etag is stale (another client
/// edited the same event between our last sync and this PUT)
/// we sync the parent calendar to pull the new etag, refetch
/// the server handle, and retry the PUT once.  The user never
/// sees the "refresh and try again" failure mode.
///
/// Caller passes the app-side `event_id` so we can refetch
/// the handle after the sync — `event_row_id` is stable across
/// syncs (`{calendar_id}::{uid}`), so the same id resolves to
/// the freshly-synced row with the new etag.
///
/// Returns the (possibly second-attempt) `WriteOutcome` and
/// the handle it was written against.  A second 412 bubbles
/// up unwrapped — that means something else (not a simple
/// stale-cache race) is in conflict, and the caller should
/// surface it.
async fn update_event_with_etag_retry(
    cache: &Cache,
    event_id: &str,
    ics: &str,
) -> Result<(nimbus_caldav::WriteOutcome, CalendarEventServerHandle), NimbusError> {
    let handle = load_event_handle(cache, event_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;

    match caldav_update_event(
        &handle.href,
        &account.username,
        &app_password,
        &handle.etag,
        ics,
        &account.trusted_certs,
    )
    .await
    {
        Ok(o) => Ok((o, handle)),
        Err(NimbusError::EtagMismatch(_)) => {
            tracing::info!("stale etag for {event_id}; refreshing calendar cache and retrying");
            let cal_path = cache
                .get_calendar_server_path(&handle.calendar_id)?
                .map(|(_, p)| p)
                .ok_or_else(|| {
                    NimbusError::Other(format!(
                        "calendar '{}' is not in the local cache",
                        handle.calendar_id
                    ))
                })?;
            refresh_calendar_cache(cache, &handle.nextcloud_account_id, &cal_path).await?;
            let fresh = load_event_handle(cache, event_id)?;
            let outcome = caldav_update_event(
                &fresh.href,
                &account.username,
                &app_password,
                &fresh.etag,
                ics,
                &account.trusted_certs,
            )
            .await?;
            Ok((outcome, fresh))
        }
        Err(e) => Err(e),
    }
}

/// Pull the latest events for one calendar via CalDAV
/// sync-collection and apply the delta to the local cache.
/// Same plumbing as `sync_nextcloud_calendars`'s inner loop, but
/// scoped to a single calendar so the inbound-RSVP path can
/// freshen its cache before deciding create-vs-update.  Soft
/// failures (server transient, no auth, anything) bubble back as
/// `Err`; the caller decides whether to fall through.
async fn refresh_calendar_cache(
    cache: &Cache,
    nc_id: &str,
    calendar_path: &str,
) -> Result<(), NimbusError> {
    let account = load_nextcloud_account(nc_id)?;
    let app_password = credentials::get_nextcloud_password(nc_id)?;
    // Look up the local calendar id by path so we can fetch its
    // sync token and apply the delta against it.
    let calendars = cache.list_calendars(nc_id)?;
    let cal = calendars
        .into_iter()
        .find(|c| c.path == calendar_path)
        .ok_or_else(|| {
            NimbusError::Other(format!(
                "calendar '{calendar_path}' is not in the local cache"
            ))
        })?;
    let prev_token = cache
        .get_calendar_sync_state(&cal.id)
        .ok()
        .flatten()
        .and_then(|s| s.sync_token);
    let delta = caldav_sync_calendar(
        &account.server_url,
        &cal.path,
        &account.username,
        &app_password,
        prev_token.as_deref(),
        &account.trusted_certs,
    )
    .await?;
    let upserts: Vec<CalendarEventRow> = delta.upserts.iter().flat_map(raw_event_to_rows).collect();
    cache.apply_event_delta(
        &cal.id,
        &upserts,
        &delta.deleted_hrefs,
        delta.new_sync_token.as_deref(),
        cal.ctag.as_deref(),
    )?;
    Ok(())
}

fn load_event_handle(
    cache: &Cache,
    event_id: &str,
) -> Result<CalendarEventServerHandle, NimbusError> {
    cache
        .get_event_server_handle(event_id)
        .map_err(NimbusError::from)?
        .ok_or_else(|| {
            NimbusError::Other(format!(
                "event '{event_id}' is not in the local cache — refresh and try again"
            ))
        })
}

/// Flatten one CalDAV resource (href-with-ics) into one store row per
/// VEVENT it contains. Master + recurrence-id overrides all share the
/// same `href`, `etag`, and `ics_raw` — `apply_event_delta` keys the
/// wipe-on-upsert by href, so re-syncing an href with fewer overrides
/// correctly removes the ones that vanished server-side.
fn raw_event_to_rows(raw: &RawEvent) -> Vec<CalendarEventRow> {
    raw.events
        .iter()
        .map(|e| CalendarEventRow {
            // The caldav parser stores the VEVENT UID in `id`.
            uid: e.id.clone(),
            recurrence_id: e.recurrence_id,
            href: raw.href.clone(),
            etag: raw.etag.clone(),
            summary: e.summary.clone(),
            description: e.description.clone(),
            start: e.start,
            end: e.end,
            location: e.location.clone(),
            rrule: e.rrule.clone(),
            rdate: e.rdate.clone(),
            exdate: e.exdate.clone(),
            url: e.url.clone(),
            transparency: e.transparency.clone(),
            attendees: e.attendees.clone(),
            reminders: e.reminders.clone(),
            latitude: e.latitude,
            longitude: e.longitude,
            ics_raw: raw.ics_raw.clone(),
        })
        .collect()
}

/// Fold a `ContactInput` into the shape `build_vcard` expects. The
/// UID is pulled from the caller because the two code paths (create
/// vs. update) source it differently — a fresh UUID vs. the cached
/// one.
fn input_to_parsed(uid: &str, input: &ContactInput) -> ParsedVcard {
    // Auto-derive FN from the structured-name parts when the
    // user filled them in but left `display_name` blank — same
    // convention every desktop contacts app uses (RFC 6350 §6.2.1
    // requires FN, but the form lets users type only the broken-
    // out pieces).  When both are present, `display_name` from
    // the form wins so an explicit override is honoured.
    let derived_fn = input
        .structured_name
        .as_ref()
        .map(|n| {
            [
                n.prefix.trim(),
                n.given.trim(),
                n.additional.trim(),
                n.family.trim(),
                n.suffix.trim(),
            ]
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<&str>>()
            .join(" ")
        })
        .unwrap_or_default();
    let fn_value = if !input.display_name.trim().is_empty() {
        input.display_name.clone()
    } else if !derived_fn.is_empty() {
        derived_fn
    } else {
        String::new()
    };
    ParsedVcard {
        uid: uid.to_string(),
        display_name: fn_value,
        emails: input
            .emails
            .iter()
            .map(|e| nimbus_carddav::VcardEmail {
                kind: e.kind.clone(),
                value: e.value.clone(),
            })
            .collect(),
        phones: input
            .phones
            .iter()
            .map(|p| nimbus_carddav::VcardPhone {
                kind: p.kind.clone(),
                value: p.value.clone(),
            })
            .collect(),
        organization: input.organization.clone(),
        photo_mime: input.photo_mime.clone(),
        photo_data: input.photo_data.clone(),
        title: input.title.clone(),
        birthday: input.birthday.clone(),
        note: input.note.clone(),
        addresses: input
            .addresses
            .as_ref()
            .map(|list| {
                list.iter()
                    .map(|a| nimbus_carddav::VcardAddress {
                        kind: a.kind.clone(),
                        street: a.street.clone(),
                        locality: a.locality.clone(),
                        region: a.region.clone(),
                        postal_code: a.postal_code.clone(),
                        country: a.country.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        urls: input.urls.clone().unwrap_or_default(),
        kind: String::new(),
        members: Vec::new(),
        categories: input.categories.clone().unwrap_or_default(),
        // ── #143 ─────────────────────────────────────────────
        structured_name: input
            .structured_name
            .as_ref()
            .map(|n| nimbus_carddav::VcardStructuredName {
                family: n.family.clone(),
                given: n.given.clone(),
                additional: n.additional.clone(),
                prefix: n.prefix.clone(),
                suffix: n.suffix.clone(),
            })
            .unwrap_or_default(),
        nickname: input.nickname.clone(),
        anniversary: input.anniversary.clone(),
        gender: input.gender.clone(),
        impp: input
            .impp
            .as_ref()
            .map(|list| {
                list.iter()
                    .map(|i| nimbus_carddav::VcardImpp {
                        kind: i.kind.clone(),
                        value: i.value.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        role: input.role.clone(),
        languages: input.languages.clone().unwrap_or_default(),
        geo: input.geo.clone(),
        timezone: input.timezone.clone(),
        keys: input.keys.clone().unwrap_or_default(),
    }
}

/// Build a `ContactRow` from a freshly-PUT vCard's outcome. Extracted
/// so create/update both ship the same set of extended fields
/// (addresses, birthday, urls, note, title) into the cache.
fn parsed_to_row(
    href: &str,
    etag: &str,
    uid: &str,
    parsed: &ParsedVcard,
    vcard_raw: String,
) -> ContactRow {
    ContactRow {
        href: href.to_string(),
        etag: etag.to_string(),
        vcard_uid: uid.to_string(),
        display_name: parsed.display_name.clone(),
        emails: parsed
            .emails
            .iter()
            .map(|e| nimbus_core::models::ContactEmail {
                kind: e.kind.clone(),
                value: e.value.clone(),
            })
            .collect(),
        phones: parsed
            .phones
            .iter()
            .map(|p| nimbus_core::models::ContactPhone {
                kind: p.kind.clone(),
                value: p.value.clone(),
            })
            .collect(),
        organization: parsed.organization.clone(),
        photo_mime: parsed.photo_mime.clone(),
        photo_data: parsed.photo_data.clone(),
        title: parsed.title.clone(),
        birthday: parsed.birthday.clone(),
        note: parsed.note.clone(),
        addresses: parsed
            .addresses
            .iter()
            .map(|a| nimbus_core::models::ContactAddress {
                kind: a.kind.clone(),
                street: a.street.clone(),
                locality: a.locality.clone(),
                region: a.region.clone(),
                postal_code: a.postal_code.clone(),
                country: a.country.clone(),
            })
            .collect(),
        urls: parsed.urls.clone(),
        vcard_raw,
        kind: parsed.kind.clone(),
        member_uids: parsed.members.clone(),
        categories: parsed.categories.clone(),
    }
}

/// Hydrate a freshly-written `ContactRow` into a UI-facing
/// `Contact`. The composite id has to match the one the store
/// uses internally (`{nc_account_id}::{vcard_uid}`) so the next
/// `get_contacts` call returns the same record.
fn row_to_contact(nc_account_id: &str, addressbook: &str, row: &ContactRow) -> Contact {
    // #143: re-parse `vcard_raw` to recover the extended vCard 4
    // fields the cache schema doesn't store as dedicated columns
    // (structured-name parts, nickname, anniversary, gender, impp,
    // role, languages, geo, timezone, keys).  Round-tripping
    // through the cached body avoids a schema migration; cost is
    // one parse per contact returned to the UI, which is
    // negligible (the parser is microseconds for a typical
    // vCard).  When parsing fails — corrupt cached body, malformed
    // server data, etc. — we fall back to defaults so the rest of
    // the contact still renders.
    let extra = nimbus_carddav::parse_vcard(&row.vcard_raw).ok();
    let structured_name = extra
        .as_ref()
        .map(|p| nimbus_core::models::StructuredName {
            family: p.structured_name.family.clone(),
            given: p.structured_name.given.clone(),
            additional: p.structured_name.additional.clone(),
            prefix: p.structured_name.prefix.clone(),
            suffix: p.structured_name.suffix.clone(),
        })
        .unwrap_or_default();
    let nickname = extra.as_ref().and_then(|p| p.nickname.clone());
    let anniversary = extra.as_ref().and_then(|p| p.anniversary.clone());
    let gender = extra.as_ref().and_then(|p| p.gender.clone());
    let impp: Vec<nimbus_core::models::ContactImpp> = extra
        .as_ref()
        .map(|p| {
            p.impp
                .iter()
                .map(|i| nimbus_core::models::ContactImpp {
                    kind: i.kind.clone(),
                    value: i.value.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let role = extra.as_ref().and_then(|p| p.role.clone());
    let languages = extra
        .as_ref()
        .map(|p| p.languages.clone())
        .unwrap_or_default();
    let geo = extra.as_ref().and_then(|p| p.geo.clone());
    let timezone = extra.as_ref().and_then(|p| p.timezone.clone());
    let keys = extra.as_ref().map(|p| p.keys.clone()).unwrap_or_default();
    Contact {
        id: format!("{nc_account_id}::{}", row.vcard_uid),
        nextcloud_account_id: nc_account_id.to_string(),
        addressbook: addressbook.to_string(),
        display_name: row.display_name.clone(),
        email: row.emails.clone(),
        phone: row.phones.clone(),
        organization: row.organization.clone(),
        photo_mime: row.photo_mime.clone(),
        photo_data: row.photo_data.clone(),
        title: row.title.clone(),
        birthday: row.birthday.clone(),
        note: row.note.clone(),
        addresses: row.addresses.clone(),
        urls: row.urls.clone(),
        kind: row.kind.clone(),
        categories: row.categories.clone(),
        structured_name,
        nickname,
        anniversary,
        gender,
        impp,
        role,
        languages,
        geo,
        timezone,
        keys,
    }
}

/// Process-wide handle to the encrypted cache.  Populated once in
/// `main()` after `Cache::open_default`, so non-IPC helpers can
/// reach the pool without every call site having to extract
/// `State<'_, Cache>` and thread `&Cache` through itself.
static GLOBAL_CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();

fn global_cache() -> Result<&'static Cache, NimbusError> {
    GLOBAL_CACHE
        .get()
        .ok_or_else(|| NimbusError::Storage("cache not initialised yet".into()))
}

fn load_nextcloud_account(nc_id: &str) -> Result<NextcloudAccount, NimbusError> {
    nextcloud_store::load_accounts(global_cache()?)?
        .into_iter()
        .find(|a| a.id == nc_id)
        .ok_or_else(|| NimbusError::Other(format!("no Nextcloud account with id '{nc_id}'")))
}

fn load_contact_handle(
    cache: &Cache,
    contact_id: &str,
) -> Result<ContactServerHandle, NimbusError> {
    cache
        .get_contact_server_handle(contact_id)
        .map_err(NimbusError::from)?
        .ok_or_else(|| {
            NimbusError::Other(format!(
                "contact '{contact_id}' is not in the local cache — refresh and try again"
            ))
        })
}

// ── IMAP commands ───────────────────────────────────────────────
//
// These are the glue between the frontend mail views and the IMAP
// client. Each command performs a full connect → query → logout
// cycle. This is simple but wasteful — every click reconnects.
// A follow-up issue will introduce connection pooling / a persistent
// session so opening an email isn't a full TCP+TLS+LOGIN round-trip.
//
// Every successful network fetch also writes through to the local
// SQLite cache (Issue #4). Today the UI still always hits the
// network; a follow-up PR will flip reads to cache-first with a
// background refresh.

/// Look up an account by ID, or return a helpful error. Takes a
/// `&Cache` because every account row now lives in SQLite (#60) and
/// we want every callsite to be explicit about which DB it's reading
/// from rather than hiding a global behind a free function.
fn load_account(cache: &Cache, id: &str) -> Result<Account, NimbusError> {
    account_store::load_accounts(cache)?
        .into_iter()
        .find(|a| a.id == id)
        .ok_or_else(|| NimbusError::Other(format!("no account with id '{id}'")))
}

/// Connect to an account's IMAP server using the stored password.
/// Includes any per-account TLS-trusted certs so a self-signed
/// server the user has previously accepted continues to validate.
async fn connect_imap(account: &Account) -> Result<ImapClient, NimbusError> {
    let password = credentials::get_imap_password(&account.id)?;
    ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await
}

/// Connect to an account's JMAP server using the stored password.
async fn connect_jmap(account: &Account) -> Result<JmapClient, NimbusError> {
    let jmap_url = account.jmap_url.as_deref().ok_or_else(|| {
        NimbusError::Other(format!(
            "Account '{}' has use_jmap=true but no jmap_url configured",
            account.id
        ))
    })?;
    let password = credentials::get_imap_password(&account.id)?;
    JmapClient::connect(jmap_url, &account.email, &password).await
}

/// Returns `true` if this account should use JMAP instead of IMAP.
fn uses_jmap(account: &Account) -> bool {
    account.use_jmap && account.jmap_url.is_some()
}

/// Fetch the newest `limit` envelopes from `folder` for the given account.
///
/// Async because the IMAP client is async (tokio task spawned by Tauri).
#[tauri::command]
async fn fetch_envelopes(
    account_id: String,
    folder: String,
    limit: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<EmailEnvelope>, NimbusError> {
    match fetch_envelopes_inner(&account_id, &folder, limit, &cache).await {
        Ok(envs) => Ok(envs),
        Err(e) => {
            tracing::error!("fetch_envelopes failed: {e}");
            Err(e)
        }
    }
}

async fn fetch_envelopes_inner(
    account_id: &str,
    folder: &str,
    limit: u32,
    cache: &Cache,
) -> Result<Vec<EmailEnvelope>, NimbusError> {
    let account = load_account(cache, account_id)?;
    let _ = poll_folder(&account, folder, limit, cache).await?;
    // The poll helper already wrote through to the cache and updated
    // the sync bookmark; we return the newest `limit` from the cache
    // rather than just the delta, because the UI expects a full list
    // regardless of whether this was an incremental or full sync.
    cache
        .get_envelopes(account_id, folder, limit)
        .map_err(Into::into)
}

/// "Load older messages" — fetch up to `limit` envelopes whose UIDs
/// are strictly less than `before_uid`, used by MailList's
/// infinite-scroll path (#194). The cold-cache `fetch_envelopes`
/// path only walks the tail of a folder, so this is the surface
/// the UI calls when the user scrolls past the loaded set and
/// wants to keep going.
///
/// IMAP path runs `UID SEARCH UID 1:<before_uid-1>`, slices the
/// top `limit` UIDs (newest among the older), then fetches just
/// those envelopes. The result is written through to the SQLite
/// cache so subsequent loads are instant. Empty return = nothing
/// older exists; the frontend stops asking.
///
/// JMAP isn't wired here yet — we tracing-warn and return an
/// empty batch so the frontend simply stops paginating.
#[tauri::command]
async fn fetch_older_envelopes(
    account_id: String,
    folder: String,
    before_uid: u32,
    limit: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<EmailEnvelope>, NimbusError> {
    let account = load_account(&cache, &account_id)?;
    if uses_jmap(&account) {
        tracing::warn!(
            "fetch_older_envelopes: JMAP older-pagination not implemented for '{account_id}'/'{folder}'"
        );
        return Ok(Vec::new());
    }

    let mut client = connect_imap(&account).await?;
    let batch = client
        .fetch_older_envelopes(&folder, before_uid, limit)
        .await?;
    let _ = client.logout().await;

    if !batch.envelopes.is_empty()
        && let Err(e) = cache.upsert_envelopes_for_account(&account_id, &batch.envelopes)
    {
        tracing::warn!("cache.upsert_envelopes (older) failed: {e}");
    }

    // Stamp the account_id into the returned envelopes so the
    // frontend's grouping logic (unified inbox uses
    // `account_id` to route per-row clicks) keeps working —
    // the IMAP method leaves it empty since it doesn't know
    // which account it's serving.
    let mut envelopes = batch.envelopes;
    for env in &mut envelopes {
        env.account_id = account_id.clone();
    }
    Ok(envelopes)
}

/// Unified-inbox version of `fetch_older_envelopes`. Each account
/// has its own UID space, so the frontend passes a per-account
/// `before_uid` map. We poll each account's folder with its own
/// anchor and merge the results. Same JMAP caveat as the
/// per-account version.
#[tauri::command]
async fn fetch_older_unified_envelopes(
    folder: String,
    before_uid_per_account: std::collections::HashMap<String, u32>,
    limit: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<EmailEnvelope>, NimbusError> {
    let accounts = account_store::load_accounts(&cache).unwrap_or_default();
    let mut merged: Vec<EmailEnvelope> = Vec::new();
    for account in &accounts {
        let Some(&before_uid) = before_uid_per_account.get(&account.id) else {
            continue;
        };
        if uses_jmap(account) {
            tracing::warn!(
                "fetch_older_unified_envelopes: JMAP not implemented for '{}'",
                account.id
            );
            continue;
        }
        let mut client = match connect_imap(account).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("unified older: connect failed for '{}': {e}", account.id);
                continue;
            }
        };
        match client
            .fetch_older_envelopes(&folder, before_uid, limit)
            .await
        {
            Ok(batch) => {
                if let Err(e) = cache.upsert_envelopes_for_account(&account.id, &batch.envelopes) {
                    tracing::warn!("cache.upsert_envelopes (unified older) failed: {e}");
                }
                for mut env in batch.envelopes {
                    env.account_id = account.id.clone();
                    merged.push(env);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "unified older fetch failed for '{}'/'{folder}': {e}",
                    account.id
                );
            }
        }
        let _ = client.logout().await;
    }
    // Newest-first, capped at the unified `limit` so a single
    // chatty account doesn't crowd the page.
    merged.sort_unstable_by_key(|e| std::cmp::Reverse(e.date));
    merged.truncate(limit as usize);
    Ok(merged)
}

/// Unified-inbox version of `fetch_envelopes`: polls every configured
/// account's `folder` (sequentially — keeps the SMTP/IMAP server load
/// predictable) and then returns the merged newest-`limit` view from
/// the cache. A poll failure on one account is logged and skipped so a
/// single broken account doesn't blank the unified list.
#[tauri::command]
async fn fetch_unified_envelopes(
    folder: String,
    limit: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<EmailEnvelope>, NimbusError> {
    let accounts = account_store::load_accounts(&cache).unwrap_or_default();
    for account in &accounts {
        if let Err(e) = poll_folder(account, &folder, limit, &cache).await {
            tracing::warn!("unified poll failed for '{}': {e}", account.id);
        }
    }
    cache
        .get_unified_envelopes(&folder, limit)
        .map_err(Into::into)
}

/// Outcome of polling a single folder — used by both the user-facing
/// `fetch_envelopes` command and the background sync loop.
///
/// Only the "new" subset is returned: the full batch is already
/// reflected in the cache via write-through, and callers that want it
/// simply `cache.get_envelopes(...)` afterwards. On the very first
/// poll (no prior sync state) `new_envelopes` is empty by design — a
/// fresh install shouldn't fire a notification for every pre-existing
/// message.
struct FolderPollOutcome {
    new_envelopes: Vec<EmailEnvelope>,
    /// Count of cached envelope rows whose `\Seen` / `\Flagged` /
    /// `\Answered` flags drifted between polls — typically because
    /// another mail client (phone, webmail) flipped a flag and we
    /// just caught up.  Callers use this to fire the
    /// `mail-flags-updated` Tauri event so the frontend can re-read
    /// the cache without a manual refresh (#255 follow-up).
    flag_changes: u32,
}

/// Fetch+cache+reconcile for one (account, folder) pair.
///
/// Shared code path for interactive refreshes and background polling.
/// Steps:
/// 1. Consult the cache for prior `SyncState` (UIDVALIDITY + highest UID).
/// 2. JMAP: one-shot fetch; there's no UIDVALIDITY, so "new" is decided
///    purely by comparing UIDs to `prior_highest`.
/// 3. IMAP: incremental fetch via `since_uid`; if UIDVALIDITY rotated,
///    wipe the folder cache and redo in full mode (no notifications on
///    rotation — `new_envelopes` stays empty in that branch).
/// 4. Write envelopes through to the cache.
/// 5. Update the sync bookmark to `max(prior, newest-fetched)` so an
///    empty incremental response can't accidentally rewind it.
async fn poll_folder(
    account: &Account,
    folder: &str,
    limit: u32,
    cache: &Cache,
) -> Result<FolderPollOutcome, NimbusError> {
    let account_id = &account.id;
    let prior = cache.get_sync_state(account_id, folder).ok().flatten();
    let prior_highest = prior.as_ref().and_then(|s| s.highest_uid_seen);

    // ── JMAP path ──────────────────────────────────────────────
    if uses_jmap(account) {
        let client = connect_jmap(account).await?;
        let envelopes = client.fetch_envelopes(folder, limit, None).await?;

        if let Err(e) = cache.upsert_envelopes_for_account(account_id, &envelopes) {
            tracing::warn!("cache.upsert_envelopes (JMAP) failed: {e}");
        }

        let new_envelopes: Vec<EmailEnvelope> = envelopes
            .iter()
            .filter(|e| prior_highest.is_some_and(|p| e.uid > p))
            .cloned()
            .collect();

        // Credit any newly-arrived unread envelopes against the
        // folder's badge so the sidebar count moves immediately on
        // the next read — without waiting for a fresh `STATUS` round
        // trip from `fetch_folders`.
        let new_unread = new_envelopes.iter().filter(|e| !e.is_read).count() as i64;
        if let Err(e) = cache.bump_folder_unread(account_id, folder, new_unread) {
            tracing::warn!("cache.bump_folder_unread (JMAP) failed: {e}");
        }

        // Bookmark UPDATE: JMAP has no UIDVALIDITY; we only track the
        // highest UID so background polls can diff.
        let new_highest = envelopes
            .iter()
            .map(|e| e.uid)
            .max()
            .into_iter()
            .chain(prior_highest)
            .max();
        let state = SyncState {
            uidvalidity: None,
            highest_uid_seen: new_highest,
            last_synced_at: Some(chrono::Utc::now()),
        };
        if let Err(e) = cache.set_sync_state(account_id, folder, &state) {
            tracing::warn!("cache.set_sync_state (JMAP) failed: {e}");
        }

        // JMAP cross-client flag refresh isn't wired here yet —
        // `Email/changes` would be the proper way, but the user's
        // primary path is IMAP and JMAP cross-client `$answered` is
        // a follow-up.  Report zero flag changes for now.
        return Ok(FolderPollOutcome {
            new_envelopes,
            flag_changes: 0,
        });
    }

    // ── IMAP path ──────────────────────────────────────────────
    let mut client = connect_imap(account).await?;
    let mut batch = client.fetch_envelopes(folder, limit, prior_highest).await?;

    // UIDVALIDITY check. If the server has rotated it, every cached UID
    // for this folder now points at a different (or deleted) message —
    // wipe the folder and redo the fetch in full mode so the cache
    // reflects reality. We also mark the outcome as rotated so the
    // caller can skip any "new mail" reactions (the UIDs aren't really
    // new — they're the same messages under a new numbering).
    let uidvalidity_rotated = matches!(
        (prior.as_ref().and_then(|s| s.uidvalidity), batch.uidvalidity),
        (Some(old), Some(new)) if old != new,
    );
    if uidvalidity_rotated {
        tracing::warn!(
            "UIDVALIDITY changed for '{account_id}'/'{folder}' \
             (was {:?}, now {:?}) — wiping folder cache",
            prior.as_ref().and_then(|s| s.uidvalidity),
            batch.uidvalidity,
        );
        if let Err(e) = cache.wipe_folder(account_id, folder) {
            tracing::warn!("cache.wipe_folder failed: {e}");
        }
        batch = client.fetch_envelopes(folder, limit, None).await?;
    }

    // Reconcile the cache against the server's live UID set. Without
    // this, any UID expunged between polls (by our own delete/archive
    // paths, by another client, or by the server itself) would linger
    // as a ghost envelope forever — the incremental fetch above only
    // ever pulls UIDs *greater* than the bookmark, it never revisits
    // older ones. Ghosts used to surface as "UID isn't in folder"
    // errors when the user clicked on them from the mail list.
    let server_uids = match client.list_all_uids(folder).await {
        Ok(uids) => uids,
        Err(e) => {
            tracing::warn!(
                "list_all_uids for '{account_id}'/'{folder}' failed (skipping reconcile): {e}"
            );
            Vec::new()
        }
    };

    // Flag refresh on the visible window (#255 follow-up).
    // `fetch_envelopes` above only fetches UIDs strictly newer than
    // the cache bookmark, so flag flips another client made
    // (mark-read on a phone, answer from webmail, star elsewhere)
    // never round-trip into Nimbus.  Cheap catch-up: one
    // `UID FETCH x,y,z (UID FLAGS)` on the same window the user
    // sees in the mail list.  Read the recent UIDs from the cache
    // *before* the upsert below — the freshly-fetched batch will
    // get its flags through the upsert path, this snapshot covers
    // everything older than the bookmark.
    let recent_cached_uids = cache
        .list_recent_envelope_uids(account_id, folder, limit)
        .unwrap_or_else(|e| {
            tracing::warn!("list_recent_envelope_uids failed (skipping flag refresh): {e}");
            Vec::new()
        });
    let flag_snapshots = if recent_cached_uids.is_empty() || uidvalidity_rotated {
        Vec::new()
    } else {
        match client.fetch_flags(folder, &recent_cached_uids).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "fetch_flags for '{account_id}'/'{folder}' failed (skipping flag refresh): {e}"
                );
                Vec::new()
            }
        }
    };

    let _ = client.logout().await;

    if !server_uids.is_empty() || uidvalidity_rotated {
        let server_set: std::collections::HashSet<u32> = server_uids.into_iter().collect();
        match cache.list_envelope_uids(account_id, folder) {
            Ok(cached_uids) => {
                let mut removed = 0u32;
                for uid in cached_uids {
                    if !server_set.contains(&uid) {
                        match cache.remove_envelope(account_id, folder, uid) {
                            Ok(true) => removed += 1,
                            Ok(false) => {}
                            Err(e) => tracing::warn!(
                                "remove_envelope (reconcile) for UID {uid} failed: {e}"
                            ),
                        }
                    }
                }
                if removed > 0 {
                    tracing::info!(
                        "Reconciled '{account_id}'/'{folder}': dropped {removed} ghost UID(s)"
                    );
                }
            }
            Err(e) => tracing::warn!("list_envelope_uids failed: {e}"),
        }
    }

    if let Err(e) = cache.upsert_envelopes_for_account(account_id, &batch.envelopes) {
        tracing::warn!("cache.upsert_envelopes failed: {e}");
    }

    let new_envelopes: Vec<EmailEnvelope> = if uidvalidity_rotated {
        Vec::new()
    } else {
        batch
            .envelopes
            .iter()
            .filter(|e| prior_highest.is_some_and(|p| e.uid > p))
            .cloned()
            .collect()
    };

    // Same idea as the JMAP path — bump the folder badge by the count
    // of newly-arrived unread envelopes so the sidebar reflects new
    // mail without a `STATUS` round trip. After a UIDVALIDITY rotation
    // `new_envelopes` is empty so `delta` is 0 and this is a no-op.
    let new_unread = new_envelopes.iter().filter(|e| !e.is_read).count() as i64;
    if let Err(e) = cache.bump_folder_unread(account_id, folder, new_unread) {
        tracing::warn!("cache.bump_folder_unread failed: {e}");
    }

    let new_highest = batch
        .envelopes
        .iter()
        .map(|e| e.uid)
        .max()
        .into_iter()
        .chain(prior_highest)
        .max();
    let state = SyncState {
        uidvalidity: batch.uidvalidity,
        highest_uid_seen: new_highest,
        last_synced_at: Some(chrono::Utc::now()),
    };
    if let Err(e) = cache.set_sync_state(account_id, folder, &state) {
        tracing::warn!("cache.set_sync_state failed: {e}");
    }

    // Apply the flag snapshot we collected before logout.  Done
    // here (after the envelope upsert) so a UID that was both new
    // *and* re-flagged won't be flickered between the two writes —
    // the upsert lands its envelope-derived flags first, the
    // reconcile then runs against everything else.  `replied_kind`
    // is preserved by `reconcile_envelope_flags` (Nimbus-only
    // metadata that IMAP can't carry).
    let flag_changes = if flag_snapshots.is_empty() {
        0
    } else {
        let tuples: Vec<(u32, bool, bool, bool)> = flag_snapshots
            .iter()
            .map(|s| (s.uid, s.is_read, s.is_starred, s.is_answered))
            .collect();
        cache
            .reconcile_envelope_flags(account_id, folder, &tuples)
            .unwrap_or_else(|e| {
                tracing::warn!("reconcile_envelope_flags failed: {e}");
                0
            })
    };
    if flag_changes > 0 {
        tracing::info!("Reconciled {flag_changes} flag change(s) for '{account_id}'/'{folder}'");
    }

    Ok(FolderPollOutcome {
        new_envelopes,
        flag_changes,
    })
}

/// Fetch a full message (headers + body) by folder + UID.
#[tauri::command]
async fn fetch_message(
    account_id: String,
    folder: String,
    uid: u32,
    cache: State<'_, Cache>,
) -> Result<Email, NimbusError> {
    match fetch_message_inner(&account_id, &folder, uid, &cache).await {
        Ok(email) => Ok(email),
        Err(e) => {
            tracing::error!("fetch_message failed: {e}");
            Err(e)
        }
    }
}

async fn fetch_message_inner(
    account_id: &str,
    folder: &str,
    uid: u32,
    cache: &Cache,
) -> Result<Email, NimbusError> {
    let account = load_account(cache, account_id)?;

    let email = if uses_jmap(&account) {
        let client = connect_jmap(&account).await?;
        client.fetch_message(folder, uid, account_id).await?
    } else {
        let mut client = connect_imap(&account).await?;
        let email = client.fetch_message(folder, uid, account_id).await?;
        let _ = client.logout().await;
        email
    };

    // Single transactional write-through: envelope + body together so the
    // two can never drift on a partial failure.
    if let Err(e) = cache.upsert_message(&email) {
        tracing::warn!("cache.upsert_message failed: {e}");
    }

    Ok(email)
}

/// Download the decoded bytes of a single attachment on a message.
///
/// The UI renders attachment metadata from the (cached or freshly
/// fetched) `Email.attachments` list, but the bytes are never shipped
/// inline — a user with a 20 MB PDF on a message would otherwise pay
/// that cost every time they open the mail. Instead the UI calls this
/// command only when the user actually clicks "Download" or
/// "Save to Nextcloud".
///
/// IMAP path: re-FETCHes the raw message body (PEEK, so unread stays
/// unread) and extracts the attachment at `part_id`. JMAP isn't
/// plumbed through yet — callers on JMAP accounts get an explicit
/// `Protocol` error instead of silently returning empty bytes.
#[tauri::command]
async fn download_email_attachment(
    account_id: String,
    folder: String,
    uid: u32,
    part_id: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<u8>, NimbusError> {
    let account = load_account(&cache, &account_id)?;
    if uses_jmap(&account) {
        return Err(NimbusError::Protocol(
            "JMAP attachment download is not implemented yet".into(),
        ));
    }
    let mut client = connect_imap(&account).await?;
    let (_meta, data) = client.fetch_attachment(&folder, uid, part_id).await?;
    let _ = client.logout().await;
    Ok(data)
}

// ── Attachment preview cache (#157) ──────────────────────────
//
// Persists frontend-generated thumbnails alongside the cached
// message body so MailView re-renders without re-fetching the
// full attachment bytes.  See nimbus-store/src/cache/mod.rs
// for the schema and helpers.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentPreviewView {
    part_id: u32,
    mime: String,
    /// Base64-encoded thumbnail bytes — the frontend pipes these
    /// straight into a `data:` URL without going through a Blob.
    base64: String,
}

/// Record a rendered thumbnail for one attachment.  Frontend
/// calls this once per attachment after AttachmentThumb extracts
/// or downsamples the preview; subsequent opens of the same
/// message read all of them back in a single query via
/// `get_attachment_previews`.
///
/// Bytes arrive base64-encoded — Tauri's default JSON serializer
/// turns a `Vec<u8>` into a `[123, 45, ...]` number array on the
/// wire, which is roughly 3× the raw size.  A base64 string is
/// ≈1.33× and decodes server-side in microseconds.
#[tauri::command]
fn put_attachment_preview(
    account_id: String,
    folder: String,
    uid: u32,
    part_id: u32,
    mime: String,
    base64: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let bytes = STANDARD
        .decode(base64.as_bytes())
        .map_err(|e| NimbusError::Other(format!("attachment preview base64 decode: {e}")))?;
    cache
        .put_attachment_preview(&account_id, &folder, uid, part_id, &mime, &bytes)
        .map_err(NimbusError::from)
}

/// Bulk-fetch every stored thumbnail for a message.  MailView
/// invokes this once when the email mounts and seeds the
/// in-memory thumb cache so no subsequent `<AttachmentThumb>`
/// has to fetch bytes or run extraction.
#[tauri::command]
fn get_attachment_previews(
    account_id: String,
    folder: String,
    uid: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<AttachmentPreviewView>, NimbusError> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let rows = cache
        .get_attachment_previews_for_message(&account_id, &folder, uid)
        .map_err(NimbusError::from)?;
    Ok(rows
        .into_iter()
        .map(|r| AttachmentPreviewView {
            part_id: r.part_id,
            mime: r.mime,
            base64: STANDARD.encode(r.bytes),
        })
        .collect())
}

/// Find an iCalendar payload anywhere in the message and return
/// its raw bytes.  Used by MailView as a fallback for invites
/// where the cached `attachments` array doesn't surface the
/// calendar — most commonly the canonical iMIP MIME shape
/// where `text/calendar` is a body alternative inside
/// `multipart/alternative` and mail-parser classifies it as a
/// body part rather than an attachment.  Returns `None` when
/// the message genuinely has no calendar content (caller hides
/// the RSVP card).
#[tauri::command]
async fn download_calendar_from_message(
    account_id: String,
    folder: String,
    uid: u32,
    cache: State<'_, Cache>,
) -> Result<Option<Vec<u8>>, NimbusError> {
    let account = load_account(&cache, &account_id)?;
    if uses_jmap(&account) {
        return Err(NimbusError::Protocol(
            "JMAP calendar extraction is not implemented yet".into(),
        ));
    }
    let mut client = connect_imap(&account).await?;
    let bytes = client.fetch_calendar_payload(&folder, uid).await?;
    let _ = client.logout().await;
    Ok(bytes)
}

/// Mark a message as read on the server and in the local cache.
///
/// Cache first so the UI sees the change immediately; then the network
/// call propagates the `\Seen` flag to the IMAP server. If the server
/// call fails, we surface the error — but the cache is already updated,
/// which is an acceptable divergence (the next sync will reconcile it).
#[tauri::command]
async fn mark_as_read(
    account_id: String,
    folder: String,
    uid: u32,
    cache: State<'_, Cache>,
    app: AppHandle,
) -> Result<(), NimbusError> {
    set_message_read(account_id, folder, uid, true, cache, app).await
}

/// Toggle the read state of a single message. Generalises
/// `mark_as_read` so the UI can also mark messages as *unread*
/// (the explicit "Mark as unread" affordance — toolbar button and
/// MailList right-click menu).
#[tauri::command]
async fn set_message_read(
    account_id: String,
    folder: String,
    uid: u32,
    read: bool,
    cache: State<'_, Cache>,
    app: AppHandle,
) -> Result<(), NimbusError> {
    // Optimistic cache update — instant UI feedback. Both the
    // `mark_envelope_*` helpers also adjust `folders.unread_count`
    // so the sidebar badge moves with the change.
    let cache_result = if read {
        cache.mark_envelope_read(&account_id, &folder, uid)
    } else {
        cache.mark_envelope_unread(&account_id, &folder, uid)
    };
    if let Err(e) = cache_result {
        tracing::warn!("cache flag update failed: {e}");
    }

    // The user's mental model is "I clicked it, the counter moved"
    // — a 5-minute sync wait would feel broken.
    refresh_unread_badge(&app);

    let account = load_account(&cache, &account_id)?;
    if uses_jmap(&account) {
        let client = connect_jmap(&account).await?;
        return if read {
            client.mark_as_read(&folder, uid).await
        } else {
            client.mark_as_unread(&folder, uid).await
        };
    }

    let mut client = connect_imap(&account).await?;
    let result = if read {
        client.mark_as_read(&folder, uid).await
    } else {
        client.mark_as_unread(&folder, uid).await
    };
    let _ = client.logout().await;
    result
}

/// Remove a message from a folder.
///
/// UX shape matches every major mail client: a first "Delete" press
/// moves the message to Trash (reversible), a second press (from
/// Trash itself, or from any folder on accounts without a Trash
/// folder) permanently expunges it.
///
/// Entry points:
///   - MailView "Delete" button → here.
///   - `save_draft` replace flow → bypasses this command and calls
///     the low-level `ImapClient::delete_message` directly, because
///     "replace this draft with a new version" is update-in-place
///     and shouldn't litter Trash with editing history.
#[tauri::command]
async fn delete_message(
    account_id: String,
    folder: String,
    uid: u32,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let account = load_account(&cache, &account_id)?;

    if uses_jmap(&account) {
        return Err(NimbusError::Other(
            "Deleting messages via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }

    // Decide move-to-Trash vs permanent. Already-in-Trash comparison
    // is case-insensitive because the folder name the frontend hands
    // us is the server-reported name but mail servers don't
    // guarantee case stability across listings.
    let trash = pick_trash_folder(&account.id, cache.inner());
    let destination = match trash.as_deref() {
        Some(trash) if !folder.eq_ignore_ascii_case(trash) => Some(trash.to_string()),
        _ => None,
    };

    let password = credentials::get_imap_password(&account.id)?;

    // Drafts-folder cleanup (#193): if this is a draft and its
    // cached body carries Compose-stamped Nextcloud share
    // markers (`data-nimbus-share-id` + `data-nimbus-share-nc`
    // on the share-link anchors), tear those shares down before
    // the message itself goes away.  Without this, deleting a
    // draft from the mail list leaves dangling entries in the
    // user's "Shared with others" list.  Best-effort — we log
    // and move on if individual deletes fail; never block the
    // message delete itself.
    if pick_drafts_folder(&account.id, cache.inner())
        .as_deref()
        .is_some_and(|d| folder.eq_ignore_ascii_case(d))
        && let Ok(Some(msg)) = cache.get_message(&account.id, &folder, uid)
    {
        let body = msg.body_html.unwrap_or_default();
        for (nc_id, share_id) in extract_managed_shares(&body) {
            let nc_id_owned = nc_id.clone();
            let share_id_owned = share_id.clone();
            if let Err(e) = (async {
                let nc_account = load_nextcloud_account(&nc_id_owned)?;
                let app_password = credentials::get_nextcloud_password(&nc_id_owned)?;
                nimbus_nextcloud::delete_share(
                    &nc_account.server_url,
                    &nc_account.username,
                    &app_password,
                    &share_id_owned,
                    &account.trusted_certs,
                )
                .await
            })
            .await
            {
                tracing::warn!(
                    "delete_message: cleanup of share nc={nc_id} id={share_id} failed: {e}"
                );
            }
        }
    }

    // Optimistic-UI tombstone (#174): mark the cache row as
    // pending-delete BEFORE the IMAP roundtrip so a folder-switch
    // mid-flight doesn't resurrect the row.  The mark survives an
    // app crash too; the next launch's reconciler will drop the
    // row if the server confirmed the delete, or a manual refresh
    // from the lock screen / menu will re-run the IMAP path.
    if let Err(e) = cache.mark_message_pending(&account_id, &folder, uid, "delete") {
        tracing::warn!("mark_message_pending(delete) failed: {e}");
    }

    let connect_result = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await;
    let mut client = match connect_result {
        Ok(c) => c,
        Err(e) => {
            // IMAP wasn't reachable at all — un-tombstone the row
            // so the next folder pull restores it.
            if let Err(c) = cache.clear_message_pending(&account_id, &folder, uid) {
                tracing::warn!("clear_message_pending after connect failure: {c}");
            }
            return Err(e);
        }
    };
    let result = match destination.as_deref() {
        Some(trash) => client.move_message(&folder, uid, trash).await,
        None => client.delete_message(&folder, uid).await,
    };
    let _ = client.logout().await;

    if result.is_err() && !should_clean_cache_for_delete(&result) {
        // True IMAP failure (not the stale-UID case the cache-
        // cleanup heuristic absorbs).  Drop the tombstone so the
        // row reappears in the user's next folder pull.
        if let Err(c) = cache.clear_message_pending(&account_id, &folder, uid) {
            tracing::warn!("clear_message_pending after IMAP failure: {c}");
        }
    }

    // Clear the cache row whether the delete succeeded OR failed with
    // "UID not on the server" — in the success case the cache would
    // otherwise hang onto a ghost row (incremental envelope fetch
    // never re-examines existing UIDs), and in the failure case the
    // reason we hit that error *is* a stale cache row, so dropping it
    // unblocks the user's next refresh.  `remove_envelope` clears the
    // pending tombstone implicitly by deleting the whole row.
    if should_clean_cache_for_delete(&result)
        && let Err(e) = cache.remove_envelope(&account_id, &folder, uid)
    {
        tracing::warn!("remove_envelope after delete_message failed: {e}");
    }

    result
}

/// Locate the account's Trash folder via the IMAP `\Trash` special-use
/// attribute or a name-based fallback. Same strategy as the Sent /
/// Drafts / Archive pickers. Returns `None` if nothing matches — the
/// delete path interprets that as "no Trash on this account, fall back
/// to permanent expunge".
fn pick_trash_folder(account_id: &str, cache: &Cache) -> Option<String> {
    let folders = cache.get_folders(account_id).ok()?;

    if let Some(by_attr) = folders.iter().find(|f| {
        f.attributes
            .iter()
            .any(|a| a.eq_ignore_ascii_case("trash") || a.eq_ignore_ascii_case("\\trash"))
    }) {
        return Some(by_attr.name.clone());
    }

    const NAME_HINTS: &[&str] = &[
        "trash",
        "bin",
        "deleted items",
        "deleted messages",
        "papierkorb",
        "corbeille",
        "[gmail]/trash",
    ];
    folders
        .iter()
        .find(|f| {
            let lower = f.name.to_lowercase();
            NAME_HINTS.iter().any(|h| lower.contains(h))
        })
        .map(|f| f.name.clone())
}

/// Did this delete_message result leave the cache holding a definitely-
/// stale row for the target UID? True when the server confirmed the
/// delete (Ok) *or* reported the UID isn't there (the probe error we
/// added to `delete_message`) — in both cases the cached envelope
/// should come out.
fn should_clean_cache_for_delete(result: &Result<(), NimbusError>) -> bool {
    match result {
        Ok(()) => true,
        Err(NimbusError::Protocol(msg)) => msg.contains("isn't in folder"),
        _ => false,
    }
}

/// Move the message to the account's Archive folder.
///
/// Semantics: single-click "I'm done with this, get it out of my
/// face" — the message is preserved on the server (unlike
/// `delete_message`) but pulled out of the current mailbox so the
/// Inbox stops showing it. If no Archive folder can be located
/// (server doesn't expose one and no common name matches) the
/// caller gets a clear error rather than silently deleting.
#[tauri::command]
async fn archive_message(
    account_id: String,
    folder: String,
    uid: u32,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let account = load_account(&cache, &account_id)?;

    if uses_jmap(&account) {
        return Err(NimbusError::Other(
            "Archiving via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }

    let Some(archive) = pick_archive_folder(&account.id, cache.inner()) else {
        return Err(NimbusError::Other(
            "no Archive folder found for this account — create one on the server or tell us which folder to use".into(),
        ));
    };

    if archive.eq_ignore_ascii_case(&folder) {
        // Already sitting in Archive. Silently succeed rather than
        // move-to-self, which some servers reject and others treat
        // as a noop with a surprising UID change.
        return Ok(());
    }

    let password = credentials::get_imap_password(&account.id)?;

    // Optimistic-UI tombstone (#174) — see `delete_message`.
    let pending = format!("move:{archive}");
    if let Err(e) = cache.mark_message_pending(&account_id, &folder, uid, &pending) {
        tracing::warn!("mark_message_pending(archive) failed: {e}");
    }

    let connect_result = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await;
    let mut client = match connect_result {
        Ok(c) => c,
        Err(e) => {
            if let Err(c) = cache.clear_message_pending(&account_id, &folder, uid) {
                tracing::warn!("clear_message_pending after archive connect failure: {c}");
            }
            return Err(e);
        }
    };
    let result = client.move_message(&folder, uid, &archive).await;
    let _ = client.logout().await;

    if result.is_ok() {
        // The envelope row for the source folder needs to go — the
        // next `fetch_envelopes` is an incremental one and won't
        // notice the move by itself.
        if let Err(e) = cache.remove_envelope(&account_id, &folder, uid) {
            tracing::warn!("remove_envelope after archive_message failed: {e}");
        }
    } else if let Err(c) = cache.clear_message_pending(&account_id, &folder, uid) {
        tracing::warn!("clear_message_pending after archive failure: {c}");
    }

    result
}

/// Move a message to an arbitrary user-picked folder (#89).
///
/// Same shape as `archive_message`, but the destination comes
/// straight from the caller — the picker UI in `MailView` and the
/// drag-and-drop handler in the sidebar both feed through here.
/// Move-to-self is a noop because some IMAP servers reject it and
/// others treat it as a UID-changing roundtrip.  JMAP accounts
/// return an error until JMAP MOVE lands.
#[tauri::command]
async fn move_message(
    account_id: String,
    folder: String,
    uid: u32,
    dest_folder: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let account = load_account(&cache, &account_id)?;

    if uses_jmap(&account) {
        return Err(NimbusError::Other(
            "Move via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }

    if dest_folder.eq_ignore_ascii_case(&folder) {
        // Move-to-self is a noop.  Don't trip the IMAP server with a
        // request it might reject, and don't bump the UID.
        return Ok(());
    }

    let password = credentials::get_imap_password(&account.id)?;

    // Optimistic-UI tombstone (#174) — see `delete_message`.
    let pending = format!("move:{dest_folder}");
    if let Err(e) = cache.mark_message_pending(&account_id, &folder, uid, &pending) {
        tracing::warn!("mark_message_pending(move) failed: {e}");
    }

    let connect_result = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await;
    let mut client = match connect_result {
        Ok(c) => c,
        Err(e) => {
            if let Err(c) = cache.clear_message_pending(&account_id, &folder, uid) {
                tracing::warn!("clear_message_pending after connect failure: {c}");
            }
            return Err(e);
        }
    };
    let result = client.move_message(&folder, uid, &dest_folder).await;
    let _ = client.logout().await;

    if result.is_ok() {
        // Drop the source-folder envelope row so the next incremental
        // `fetch_envelopes` doesn't have to.  The destination folder
        // will pick up the new envelope on its next sync tick.
        if let Err(e) = cache.remove_envelope(&account_id, &folder, uid) {
            tracing::warn!("remove_envelope after move_message failed: {e}");
        }
    } else if let Err(c) = cache.clear_message_pending(&account_id, &folder, uid) {
        tracing::warn!("clear_message_pending after move failure: {c}");
    }

    result
}

/// Batch variant of `move_message` (#89): every message in `uids`
/// moves from the same source folder to the same destination on a
/// single IMAP session.  Issues the UID COPY + UID STORE with a
/// comma-joined UID set so the server handles the lot in one
/// round-trip, and EXPUNGEs once at the end.  Per-call
/// connect/login/logout overhead drops from N to 1, and we no
/// longer race per-message connections — the previous "loop in JS
/// + invoke per UID" flow lost the last move on some servers due
/// to rapid connection recycling.
///
/// Returns the list of UIDs the cache + server agree are gone, so
/// the JS caller can fire its post-move callbacks against a
/// definite success set.
#[tauri::command]
async fn move_messages(
    account_id: String,
    folder: String,
    uids: Vec<u32>,
    dest_folder: String,
    cache: State<'_, Cache>,
) -> Result<Vec<u32>, NimbusError> {
    if uids.is_empty() {
        return Ok(vec![]);
    }
    let account = load_account(&cache, &account_id)?;

    if uses_jmap(&account) {
        return Err(NimbusError::Other(
            "Move via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }

    if dest_folder.eq_ignore_ascii_case(&folder) {
        return Ok(vec![]); // move-to-self noop
    }

    let password = credentials::get_imap_password(&account.id)?;

    // Optimistic-UI tombstones (#174) — see `delete_message` for
    // the lifecycle.  Marking each UID before the IMAP roundtrip
    // means a folder switch mid-batch won't briefly show the
    // moved rows in their old folder.
    let pending = format!("move:{dest_folder}");
    for uid in &uids {
        if let Err(e) = cache.mark_message_pending(&account_id, &folder, *uid, &pending) {
            tracing::warn!("mark_message_pending(move-batch) failed: {e}");
        }
    }

    let connect_result = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await;
    let mut client = match connect_result {
        Ok(c) => c,
        Err(e) => {
            for uid in &uids {
                if let Err(c) = cache.clear_message_pending(&account_id, &folder, *uid) {
                    tracing::warn!("clear_message_pending after batch connect failure: {c}");
                }
            }
            return Err(e);
        }
    };
    let result = client
        .move_messages_batch(&folder, &uids, &dest_folder)
        .await;
    let _ = client.logout().await;

    if let Err(e) = result {
        // IMAP failed — un-tombstone every UID so the next list
        // pull restores them.
        for uid in &uids {
            if let Err(c) = cache.clear_message_pending(&account_id, &folder, *uid) {
                tracing::warn!("clear_message_pending after batch failure: {c}");
            }
        }
        return Err(e);
    }

    // Drop the source-folder envelope rows for each successful UID so
    // the next incremental `fetch_envelopes` doesn't have to.  The
    // batch IMAP command is all-or-nothing — either every UID moved
    // or the whole call returned an error — so once we get here the
    // entire input set is on the destination side.
    for uid in &uids {
        if let Err(e) = cache.remove_envelope(&account_id, &folder, *uid) {
            tracing::warn!("remove_envelope after move_messages failed: {e}");
        }
    }

    Ok(uids)
}

/// Locate the account's Archive folder via the IMAP `\Archive`
/// special-use attribute or a name-based fallback. Same strategy as
/// `pick_sent_folder` / `pick_drafts_folder`.
fn pick_archive_folder(account_id: &str, cache: &Cache) -> Option<String> {
    let folders = cache.get_folders(account_id).ok()?;

    if let Some(by_attr) = folders.iter().find(|f| {
        f.attributes
            .iter()
            .any(|a| a.eq_ignore_ascii_case("archive") || a.eq_ignore_ascii_case("\\archive"))
    }) {
        return Some(by_attr.name.clone());
    }

    const NAME_HINTS: &[&str] = &[
        "archive",
        "archiv",
        "archives",
        "archivé",
        "archivés",
        "all mail",
        "[gmail]/all mail",
    ];
    folders
        .iter()
        .find(|f| {
            let lower = f.name.to_lowercase();
            NAME_HINTS.iter().any(|h| lower.contains(h))
        })
        .map(|f| f.name.clone())
}

// ── SMTP commands ───────────────────────────────────────────────

/// Reference to the original message a Compose send is responding to
/// (#255).  Set by Compose's reply / reply-all / "respond with
/// meeting" flows so the backend can flip the IMAP `\Answered` flag
/// (or JMAP `$answered` keyword) on the original and stamp the
/// per-kind `replied_kind` into the local cache, which drives the
/// reply-icon prefix on the mail-list row.  `None` for fresh
/// composes / forwards / drafts — none of which are "answers".
///
/// `Serialize` so the Outbox (#276) can stash this alongside the
/// queued `OutgoingEmail` and replay it on a successful drain
/// retry.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RepliedToRef {
    folder: String,
    uid: u32,
    /// `"reply"` / `"reply-all"` / `"meeting"`.  Anything else falls
    /// through to a generic answered icon — the validation here is
    /// loose because the backend treats this as opaque metadata.
    kind: String,
}

/// Source row for the edit-from-outbox flow (#276).  Tells
/// `send_email` "I'm replacing the queued row with this id" —
/// the row is removed before the new copy is enqueued so the
/// queue never holds two versions of the same message during a
/// resend.  Optional on every send; absent for ordinary sends
/// (compose / reply / forward) and for retries that re-fire
/// the existing queued row in place.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutboxSourceRef {
    id: i64,
}

/// Pre-computed display fields for an Outbox row.  Cheap to render
/// straight onto the row without re-deserialising the full
/// `OutgoingEmail` JSON for every list refresh.
fn outbox_display_fields(email: &OutgoingEmail) -> (String, String, String) {
    let to_display = email.to.join(", ");
    (email.from.clone(), to_display, email.subject.clone())
}

/// Send an email via the account's configured SMTP server (#276).
///
/// **Always queue first.**  Every send routes through the local
/// `outbox_messages` table before touching SMTP.  Validation
/// (build the lettre `Message`) runs synchronously so the user
/// still gets a Compose-modal error for malformed addresses; the
/// row is then enqueued and a Tokio task spawned to attempt the
/// drain.  On a healthy network the drain finishes in the same
/// tick and the row never paints in the UI; on failure the row
/// stays for the periodic retry sweep in `background_sync_loop`.
///
/// The post-send work that used to live here (Sent APPEND,
/// answered-flag flip, JMAP send) is factored into
/// `try_drain_outbox_entry` — the spawned task and the retry
/// sweep call into the same helper so the success path is
/// identical regardless of when the drain fires.
///
/// `replied_to` (#255) is preserved through queue + retry so a
/// reply that takes a few sweeps to land still flips `\Answered`
/// on the original message.
///
/// `outbox_source` (#276 follow-up) carries the id of a queued
/// row this send is replacing — the edit-from-outbox path.  When
/// set, the row is removed before the new copy is enqueued so
/// the queue never briefly holds both versions.  Cancelling
/// Compose never reaches this command, so the original row stays
/// put on cancel.
#[tauri::command]
async fn send_email(
    account_id: String,
    email: OutgoingEmail,
    replied_to: Option<RepliedToRef>,
    outbox_source: Option<OutboxSourceRef>,
    cache: State<'_, Cache>,
    app: AppHandle,
) -> Result<i64, NimbusError> {
    // Validate up-front: building the lettre Message rejects bad
    // addresses, missing bodies, etc.  Doing it here means
    // user-facing input errors still surface in Compose's modal
    // rather than landing silently in the Outbox.  The IMAP /
    // JMAP routing decision (uses_jmap) doesn't care — both paths
    // need a valid OutgoingEmail.
    let _ = build_outgoing_message(&email)?;

    let (from_header, to_display, subject) = outbox_display_fields(&email);
    let outgoing_json = serde_json::to_string(&email)
        .map_err(|e| NimbusError::Other(format!("serialize OutgoingEmail for outbox: {e}")))?;
    let replied_to_json =
        match replied_to.as_ref() {
            Some(rt) => Some(serde_json::to_string(rt).map_err(|e| {
                NimbusError::Other(format!("serialize RepliedToRef for outbox: {e}"))
            })?),
            None => None,
        };

    // #276 follow-up — drop the source row before enqueueing the
    // edit so the queue holds at most one copy of this message at
    // any moment.  Idempotent: a `remove_outbox` for an id that's
    // already drained / been deleted is a no-op (zero rows
    // affected, no error).  Done before the new INSERT so a
    // failure in this branch can't leak a duplicate.
    if let Some(src) = outbox_source.as_ref() {
        if let Err(e) = cache.remove_outbox(src.id) {
            tracing::warn!("remove source outbox row {} failed: {e}", src.id);
        }
    }

    let entry_id = cache.enqueue_outbox(&nimbus_store::OutboxEnqueue {
        account_id: account_id.clone(),
        outgoing_json,
        replied_to_json,
        from_header,
        to_display,
        subject,
        skip_sent_copy: email.skip_sent_copy,
    })?;

    // Tell the frontend the queue grew so the synthetic Outbox
    // folder appears in the sidebar (no-op when the drain task
    // beats us — the row has already been removed by the time the
    // listener acts).
    emit_outbox_updated(&app);

    // Kick off the drain attempt immediately on a background
    // task.  The task captures its own AppHandle clone so it
    // outlives this command's return.  Cheap: ~tens of
    // microseconds per spawn.
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        let cache = app_clone.state::<Cache>();
        try_drain_outbox_entry(&app_clone, &cache, entry_id).await;
    });

    // #276 follow-up: return the new row id so Compose can hand
    // it to App.svelte's `onsentenqueued` callback.  The
    // edit-from-outbox path uses it to surface the new (or
    // still-failing) row in the right pane immediately, so the
    // user sees their edit in the queue without manually
    // re-clicking the row.
    Ok(entry_id)
}

/// Drive one queued outbox row through SMTP / JMAP.  Removes the
/// row on success, records the error on failure (the row stays
/// for the next sweep).  Used by:
///
///   * the spawned task `send_email` kicks off after enqueue,
///   * the `retry_outbox_entry` Tauri command (manual retry from
///     the UI),
///   * the periodic drain sweep in `background_sync_loop`.
///
/// Best-effort by design: any failure in the post-send Sent
/// APPEND / answered-flag flip is logged and the row is still
/// removed (the SMTP succeeded, the user's mail is out, the
/// missing local-side bookkeeping will reconcile on the next
/// envelope fetch).
async fn try_drain_outbox_entry(app: &AppHandle, cache: &Cache, entry_id: i64) {
    let row = match cache.get_outbox(entry_id) {
        Ok(Some(r)) => r,
        Ok(None) => return, // Already removed (manual delete, race with another drain).
        Err(e) => {
            tracing::warn!("get_outbox({entry_id}) failed: {e}");
            return;
        }
    };

    let email: OutgoingEmail = match serde_json::from_str(&row.outgoing_json) {
        Ok(e) => e,
        Err(e) => {
            // Hard-failed deserialise — almost certainly a schema
            // change upstream.  Record the error so the user can
            // see it on the row and decide to delete; don't keep
            // retrying forever on a malformed row.
            let msg = format!("malformed outbox payload: {e}");
            if let Err(c) = cache.record_outbox_failure(entry_id, &msg) {
                tracing::warn!("record_outbox_failure failed: {c}");
            }
            return;
        }
    };
    let replied_to: Option<RepliedToRef> =
        row.replied_to_json
            .as_deref()
            .and_then(|s| match serde_json::from_str(s) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("malformed outbox replied_to_json: {e}");
                    None
                }
            });

    let account = match load_account(cache, &row.account_id) {
        Ok(a) => a,
        Err(e) => {
            // Account was removed while a row was queued.  Drop
            // the row — there's nowhere to send from.
            tracing::warn!(
                "outbox drain dropping row {entry_id}: account '{}' missing: {e}",
                row.account_id
            );
            let _ = cache.remove_outbox(entry_id);
            emit_outbox_updated(app);
            return;
        }
    };

    let send_result: Result<(), NimbusError> =
        run_send_pipeline(app, cache, &account, &email, replied_to.as_ref()).await;

    match send_result {
        Ok(()) => {
            if let Err(e) = cache.remove_outbox(entry_id) {
                tracing::warn!("remove_outbox after success failed: {e}");
            }
            emit_outbox_updated(app);
        }
        Err(e) => {
            let msg = format!("{e}");
            tracing::info!(
                "outbox drain for entry {entry_id} (account '{}') failed: {msg}",
                row.account_id
            );
            if let Err(c) = cache.record_outbox_failure(entry_id, &msg) {
                tracing::warn!("record_outbox_failure failed: {c}");
            }
            emit_outbox_updated(app);
        }
    }
}

/// Inner send pipeline shared by `try_drain_outbox_entry` (every
/// outbox attempt) and any future direct-send caller.  Mirrors the
/// pre-#276 `send_email` body verbatim — JMAP path returns after
/// `client.send_email`, IMAP path runs SMTP + best-effort Sent
/// APPEND + best-effort answered-flag flip.
async fn run_send_pipeline(
    app: &AppHandle,
    cache: &Cache,
    account: &Account,
    email: &OutgoingEmail,
    replied_to: Option<&RepliedToRef>,
) -> Result<(), NimbusError> {
    if uses_jmap(account) {
        let client = connect_jmap(account).await?;
        client.send_email(email).await?;
        if let Some(rt) = replied_to {
            mark_original_answered_jmap(account, cache, &client, rt).await;
            emit_mail_flags_updated(app, &account.id, &rt.folder);
        }
        return Ok(());
    }

    // Build the lettre message once so the same bytes go to both
    // the SMTP recipients and the IMAP `APPEND` to Sent.  Avoids
    // the body diverging between the two paths if MIME generation
    // ever becomes non-deterministic.
    let message = build_outgoing_message(email)?;
    let raw = message.formatted();

    let password = credentials::get_imap_password(&account.id)?;
    let smtp = SmtpClient::connect(
        &account.smtp_host,
        account.smtp_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await?;
    smtp.send(email).await?;

    // Best-effort APPEND to Sent (same behaviour as before #276):
    // the user's mail is already out, a failure here is logged
    // but doesn't roll the send back.
    if !email.skip_sent_copy
        && let Err(e) = append_to_sent(account, &raw, cache).await
    {
        tracing::warn!(
            "Sent OK but failed to append a copy to Sent for account '{}': {e}",
            account.id
        );
    }

    if let Some(rt) = replied_to {
        mark_original_answered_imap(account, cache, rt).await;
        emit_mail_flags_updated(app, &account.id, &rt.folder);
    }
    Ok(())
}

/// `outbox-updated` event payload (#276).  Fires whenever the
/// queue changes shape (enqueue / drain success / drain failure /
/// manual delete) so the frontend can re-read counts and refresh
/// the synthetic Outbox folder.
#[derive(Debug, Clone, serde::Serialize)]
struct OutboxUpdatedPayload {
    /// Total queued rows across every account — drives the
    /// "show / hide synthetic Outbox folder" decision in the
    /// sidebar.
    total: u32,
}

/// Fire `outbox-updated` so the frontend re-reads the queue.
/// Best-effort — a dropped event just means the user has to wait
/// for the next sync tick to see the new state.
fn emit_outbox_updated(app: &AppHandle) {
    let total = match app.state::<Cache>().count_outbox() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("count_outbox for event payload failed: {e}");
            return;
        }
    };
    let payload = OutboxUpdatedPayload { total };
    if let Err(e) = app.emit("outbox-updated", &payload) {
        tracing::warn!("failed to emit outbox-updated event: {e}");
    }
}

/// Frontend-facing shape of one Outbox row (#276).  The serde
/// rename keeps the JS side reading camelCase fields without
/// the Rust side caring about the wire format.  `outgoing` is
/// the full `OutgoingEmail` re-deserialised from
/// `outgoing_json` so the frontend can hand it straight back to
/// Compose for the edit flow without parsing.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OutboxRowDto {
    id: i64,
    account_id: String,
    from_header: String,
    to_display: String,
    subject: String,
    queued_at: i64,
    attempt_count: u32,
    last_attempt_at: Option<i64>,
    last_error: Option<String>,
    skip_sent_copy: bool,
    /// Full `OutgoingEmail` JSON.  Parsed on the frontend by
    /// `edit_outbox_entry`'s caller; opaque on the list view.
    outgoing_json: String,
    replied_to_json: Option<String>,
}

fn dto_from_row(row: nimbus_store::OutboxRow) -> OutboxRowDto {
    OutboxRowDto {
        id: row.id,
        account_id: row.account_id,
        from_header: row.from_header,
        to_display: row.to_display,
        subject: row.subject,
        queued_at: row.queued_at,
        attempt_count: row.attempt_count,
        last_attempt_at: row.last_attempt_at,
        last_error: row.last_error,
        skip_sent_copy: row.skip_sent_copy,
        outgoing_json: row.outgoing_json,
        replied_to_json: row.replied_to_json,
    }
}

/// Per-account Outbox list (#276).  Used by the Outbox MailList
/// variant to render the queue.
#[tauri::command]
async fn list_outbox(
    account_id: String,
    cache: State<'_, Cache>,
) -> Result<Vec<OutboxRowDto>, NimbusError> {
    let rows = cache.list_outbox(&account_id)?;
    Ok(rows.into_iter().map(dto_from_row).collect())
}

/// Outbox list across every account (#276).  Used by unified-inbox
/// mode and by anything that needs the global queue (e.g. a tray
/// "queued mail" indicator).
#[tauri::command]
async fn list_all_outbox(cache: State<'_, Cache>) -> Result<Vec<OutboxRowDto>, NimbusError> {
    let rows = cache.list_all_outbox()?;
    Ok(rows.into_iter().map(dto_from_row).collect())
}

/// Total queued rows across every account.  Cheap aggregate
/// query — used by the Sidebar's "show synthetic Outbox folder?"
/// decision.
#[tauri::command]
async fn count_outbox(cache: State<'_, Cache>) -> Result<u32, NimbusError> {
    Ok(cache.count_outbox()?)
}

/// Force a drain attempt on a specific row (#276).  Used by the
/// "Retry now" button in the Outbox row UI.  Same code path the
/// background sweep uses — succeeds, fails, or no-ops if the row
/// vanished.  Doesn't block: the actual SMTP work runs on a
/// spawned task so the UI returns instantly.
#[tauri::command]
async fn retry_outbox_entry(id: i64, app: AppHandle) -> Result<(), NimbusError> {
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        let cache = app_clone.state::<Cache>();
        try_drain_outbox_entry(&app_clone, &cache, id).await;
    });
    Ok(())
}

/// Drop a queued row without sending (#276).  Used by the
/// "Delete" button in the Outbox row UI.  Idempotent — deleting
/// a row that's already drained is a no-op.
#[tauri::command]
async fn delete_outbox_entry(
    id: i64,
    cache: State<'_, Cache>,
    app: AppHandle,
) -> Result<(), NimbusError> {
    cache.remove_outbox(id)?;
    emit_outbox_updated(&app);
    Ok(())
}

/// Pull a queued row's `OutgoingEmail` (and replied-to ref) for
/// re-opening in Compose (#276).  Removes the row from the queue
/// — the new send Compose triggers will create a fresh row.  If
/// the user cancels Compose without sending, the original
/// content is gone; the user can resend manually if needed.
#[tauri::command]
async fn edit_outbox_entry(
    id: i64,
    cache: State<'_, Cache>,
    app: AppHandle,
) -> Result<OutboxRowDto, NimbusError> {
    let row = cache
        .get_outbox(id)?
        .ok_or_else(|| NimbusError::Other(format!("outbox row {id} not found")))?;
    cache.remove_outbox(id)?;
    emit_outbox_updated(&app);
    Ok(dto_from_row(row))
}

/// Fire the `mail-flags-updated` Tauri event so the frontend
/// re-reads the cache and the mail list reflects a flag change
/// without a manual refresh.  Best-effort — a dropped event just
/// means the user has to click refresh, which they would have
/// before this plumbing existed anyway.
fn emit_mail_flags_updated(app: &AppHandle, account_id: &str, folder: &str) {
    let payload = MailFlagsUpdatedPayload {
        account_id: account_id.to_string(),
        folder: folder.to_string(),
    };
    if let Err(e) = app.emit("mail-flags-updated", &payload) {
        tracing::warn!("failed to emit mail-flags-updated event: {e}");
    }
}

/// Best-effort: stamp the local cache row + the IMAP `\Answered`
/// flag on the original message that a Compose reply just answered
/// (#255).  Logs on failure rather than propagating — the user's
/// mail already left the building.
async fn mark_original_answered_imap(account: &Account, cache: &Cache, rt: &RepliedToRef) {
    if let Err(e) = cache.mark_envelope_replied(&account.id, &rt.folder, rt.uid, &rt.kind) {
        tracing::warn!(
            "answered-cache update failed for account '{}', folder '{}', uid {}: {e}",
            account.id,
            rt.folder,
            rt.uid
        );
    }

    let password = match credentials::get_imap_password(&account.id) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("answered-flag IMAP STORE skipped — keychain lookup failed: {e}");
            return;
        }
    };
    let mut client = match ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("answered-flag IMAP STORE skipped — connect failed: {e}");
            return;
        }
    };
    if let Err(e) = client.mark_as_answered(&rt.folder, rt.uid).await {
        tracing::warn!(
            "answered-flag IMAP STORE failed for '{}' uid {}: {e}",
            rt.folder,
            rt.uid
        );
    }
    let _ = client.logout().await;
}

/// JMAP analogue of `mark_original_answered_imap` — uses the
/// already-connected JMAP client (no second connect needed since
/// JMAP is HTTPS-pooled, not a long-lived session).
async fn mark_original_answered_jmap(
    account: &Account,
    cache: &Cache,
    client: &JmapClient,
    rt: &RepliedToRef,
) {
    if let Err(e) = cache.mark_envelope_replied(&account.id, &rt.folder, rt.uid, &rt.kind) {
        tracing::warn!(
            "answered-cache update failed for account '{}', folder '{}', uid {}: {e}",
            account.id,
            rt.folder,
            rt.uid
        );
    }
    if let Err(e) = client.mark_as_answered(&rt.folder, rt.uid).await {
        tracing::warn!(
            "answered-keyword JMAP set failed for '{}' uid {}: {e}",
            rt.folder,
            rt.uid
        );
    }
}

/// Locate the account's Sent folder (via the IMAP `\Sent` attribute,
/// or a name-based fallback) and `APPEND` the raw RFC 822 bytes there.
/// Marked `\Seen` so it doesn't add to the unread badge.
async fn append_to_sent(account: &Account, raw: &[u8], cache: &Cache) -> Result<(), NimbusError> {
    let sent_folder = pick_sent_folder(&account.id, cache);
    let Some(sent) = sent_folder else {
        return Err(NimbusError::Other(
            "no Sent folder found in cached folder list".into(),
        ));
    };

    let password = credentials::get_imap_password(&account.id)?;
    let mut client = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await?;
    let result = client.append_message(&sent, raw, &["\\Seen"]).await;
    let _ = client.logout().await;
    result
}

/// Payload for the "this save replaces an existing draft" flow.
/// When Compose opens an existing draft for editing, the frontend
/// hands the source UID + folder back here so `save_draft` can
/// APPEND-then-delete inside the same IMAP session — avoiding the
/// split-connection race where a separate `delete_message` call
/// would run after the APPEND and sometimes leave the original
/// behind.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftReplaceSource {
    folder: String,
    uid: u32,
}

/// Save an in-progress message to the account's IMAP Drafts folder.
///
/// Mirrors `send_email` structurally (same `OutgoingEmail` input, same
/// MIME builder) but skips SMTP entirely — the point is to hand the
/// message to the server so it shows up in the Drafts mailbox across
/// devices and the user can finish / send it later. IMAP-only for now;
/// JMAP accounts get a clear error until the equivalent `Email/set`
/// create-in-Drafts flow is wired up (tracked separately).
///
/// When `replace_source` is set, the save is treated as a
/// continuation of an existing draft the user opened from Drafts:
/// we APPEND the new copy into that *same folder* (not whatever
/// `pick_drafts_folder` thinks Drafts is — the server might have
/// multiple drafts-like folders and we want the edit to land where
/// the user is looking) and then EXPUNGE the source UID in the
/// same session, so from the user's perspective the draft they
/// were editing is updated in place.
#[tauri::command]
async fn save_draft(
    account_id: String,
    email: OutgoingEmail,
    replace_source: Option<DraftReplaceSource>,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let account = load_account(&cache, &account_id)?;

    if uses_jmap(&account) {
        return Err(NimbusError::Other(
            "Saving drafts via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }

    let message = build_outgoing_message(&email)?;
    let raw = message.formatted();

    // Prefer the source folder when replacing an existing draft so
    // APPEND and DELETE both target the folder the user actually
    // opened the draft from. Otherwise fall back to the "find the
    // account's Drafts folder" heuristic for brand-new drafts.
    let target_folder = match replace_source.as_ref() {
        Some(src) => src.folder.clone(),
        None => pick_drafts_folder(&account.id, cache.inner()).ok_or_else(|| {
            NimbusError::Other("no Drafts folder found in cached folder list".into())
        })?,
    };

    let password = credentials::get_imap_password(&account.id)?;
    let mut client = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await?;

    // `\Draft` marks the message as an unfinished draft. `\Seen`
    // keeps it out of the unread badge — there's no point notifying
    // the user about a mail they themselves just composed.
    let append_result = client
        .append_message(&target_folder, &raw, &["\\Draft", "\\Seen"])
        .await;

    // Only attempt the delete if the APPEND actually succeeded —
    // otherwise a flaky APPEND would have us destroy the user's
    // only remaining copy. We also want to clear the cached envelope
    // for the source UID whether the server-side delete hit an
    // existing UID or complained that the UID wasn't there (ghost
    // envelope left over from a previous expunge) — either way the
    // cached row is wrong and hanging onto it just makes the next
    // edit attempt fail the same way.
    let result = if append_result.is_ok() {
        if let Some(src) = replace_source {
            let delete_result = client.delete_message(&src.folder, src.uid).await;
            if should_clean_cache_for_delete(&delete_result)
                && let Err(e) = cache.remove_envelope(&account_id, &src.folder, src.uid)
            {
                tracing::warn!("remove_envelope after save_draft replace failed: {e}");
            }
            match delete_result {
                Ok(()) => Ok(()),
                Err(e) => Err(NimbusError::Other(format!(
                    "Draft saved, but removing the previous copy (UID {}) failed: {e}",
                    src.uid
                ))),
            }
        } else {
            Ok(())
        }
    } else {
        append_result
    };

    let _ = client.logout().await;
    result
}

/// Pick the most likely Drafts folder name from the cached folder list.
/// Same strategy as `pick_sent_folder`: prefer the IMAP `\Drafts`
/// special-use attribute, fall back to common English / German / French
/// names so accounts that haven't been synced yet still land in the
/// right place.
fn pick_drafts_folder(account_id: &str, cache: &Cache) -> Option<String> {
    let folders = cache.get_folders(account_id).ok()?;

    if let Some(by_attr) = folders.iter().find(|f| {
        f.attributes
            .iter()
            .any(|a| a.eq_ignore_ascii_case("drafts") || a.eq_ignore_ascii_case("\\drafts"))
    }) {
        return Some(by_attr.name.clone());
    }

    const NAME_HINTS: &[&str] = &[
        "drafts",
        "draft",
        "entwürfe",
        "entwurf",
        "brouillons",
        "brouillon",
    ];
    folders
        .iter()
        .find(|f| {
            let lower = f.name.to_lowercase();
            NAME_HINTS.iter().any(|h| lower.contains(h))
        })
        .map(|f| f.name.clone())
}

/// Pick the most likely Sent folder name from the cached folder list.
/// Prefers folders flagged with the IMAP `\Sent` special-use attribute
/// (the canonical, locale-independent answer) and falls back to common
/// English / German / French names so accounts that haven't been
/// re-synced after their first launch still get a copy filed somewhere
/// sensible. Returns `None` if nothing matches — the caller surfaces
/// that as a warning rather than an error.
fn pick_sent_folder(account_id: &str, cache: &Cache) -> Option<String> {
    let folders = cache.get_folders(account_id).ok()?;

    if let Some(by_attr) = folders.iter().find(|f| {
        f.attributes
            .iter()
            .any(|a| a.eq_ignore_ascii_case("sent") || a.eq_ignore_ascii_case("\\sent"))
    }) {
        return Some(by_attr.name.clone());
    }

    const NAME_HINTS: &[&str] = &[
        "sent",
        "sent items",
        "sent messages",
        "sent mail",
        "gesendet",
        "gesendete elemente",
        "envoyés",
    ];
    folders
        .iter()
        .find(|f| {
            let lower = f.name.to_lowercase();
            NAME_HINTS.iter().any(|h| lower.contains(h))
        })
        .map(|f| f.name.clone())
}

// ── Folder commands ─────────────────────────────────────────────

/// List the account's mailboxes live from the server and write-through
/// into the cache. Called by the Sidebar's refresh path after the
/// cache-first render.
#[tauri::command]
async fn fetch_folders(
    account_id: String,
    cache: State<'_, Cache>,
) -> Result<Vec<Folder>, NimbusError> {
    let account = load_account(&cache, &account_id)?;

    let folders = if uses_jmap(&account) {
        let client = connect_jmap(&account).await?;
        client.list_folders().await?
    } else {
        let mut client = connect_imap(&account).await?;
        let folders = client.list_folders().await?;
        let _ = client.logout().await;
        folders
    };

    // Write-through — cache failures are non-fatal; the live list is
    // still returned so the UI can render something useful.
    if let Err(e) = cache.upsert_folders(&account_id, &folders) {
        tracing::warn!("cache.upsert_folders failed: {e}");
    }
    Ok(folders)
}

#[tauri::command]
fn get_cached_folders(
    account_id: String,
    cache: State<'_, Cache>,
) -> Result<Vec<Folder>, NimbusError> {
    cache.get_folders(&account_id).map_err(Into::into)
}

// ── Folder-management commands ──────────────────────────────────
//
// Thin wrappers around the IMAP CREATE / DELETE / RENAME primitives.
// JMAP-only accounts get a not-yet-implemented error so we're never
// surprised by a silent no-op on those; the JMAP side would use
// `Mailbox/set` and is deferred.

/// Create a new mailbox. Hierarchy is expressed in the `name`
/// argument itself (e.g. `"Projects/2026"` with the server's
/// delimiter) — the caller decides whether this is top-level or a
/// subfolder, we just forward to IMAP. After success the frontend
/// re-runs `fetch_folders` so the new entry shows up.
#[tauri::command]
async fn create_folder(
    account_id: String,
    name: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let account = load_account(&cache, &account_id)?;
    if uses_jmap(&account) {
        return Err(NimbusError::Other(
            "Creating folders via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }
    let mut client = connect_imap(&account).await?;
    let result = client.create_folder(&name).await;
    let _ = client.logout().await;
    result
}

/// Delete a mailbox. The IMAP server usually refuses to drop a
/// non-empty folder (errors bubble up unchanged). On success we
/// wipe the folder's cache rows so the sidebar / MailList don't
/// keep showing ghost envelopes until the next reconcile.
#[tauri::command]
async fn delete_folder(
    account_id: String,
    name: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let account = load_account(&cache, &account_id)?;
    if uses_jmap(&account) {
        return Err(NimbusError::Other(
            "Deleting folders via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }
    let mut client = connect_imap(&account).await?;
    let result = client.delete_folder(&name).await;
    let _ = client.logout().await;

    if result.is_ok()
        && let Err(e) = cache.wipe_folder(&account_id, &name)
    {
        tracing::warn!("wipe_folder after delete_folder failed: {e}");
    }

    result
}

/// Rename a mailbox. IMAP RENAME preserves UIDs, so we carry every
/// cached envelope / body / sync bookmark over to the new name in
/// one SQL pass via `Cache::rename_folder` — no re-fetching.
#[tauri::command]
async fn rename_folder(
    account_id: String,
    old_name: String,
    new_name: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    let account = load_account(&cache, &account_id)?;
    if uses_jmap(&account) {
        return Err(NimbusError::Other(
            "Renaming folders via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }
    let mut client = connect_imap(&account).await?;
    let result = client.rename_folder(&old_name, &new_name).await;
    let _ = client.logout().await;

    if result.is_ok()
        && let Err(e) = cache.rename_folder(&account_id, &old_name, &new_name)
    {
        tracing::warn!("cache.rename_folder failed: {e}");
    }

    result
}

// ── Cache-first read commands ───────────────────────────────────
//
// These return whatever's in the local cache instantly so the UI has
// something to show on launch. The frontend pairs each call with the
// matching network `fetch_*` and replaces the view when fresh data
// lands. Returning `Option`/empty `Vec` (rather than an error) keeps
// the "cache miss is normal" path cheap.

#[tauri::command]
fn get_cached_envelopes(
    account_id: String,
    folder: String,
    limit: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<EmailEnvelope>, NimbusError> {
    cache
        .get_envelopes(&account_id, &folder, limit)
        .map_err(Into::into)
}

/// Cache-only sibling of `fetch_unified_envelopes` — returns the merged
/// newest-`limit` envelopes across all accounts without hitting the
/// network. Powers the instant first-paint of the unified inbox.
#[tauri::command]
fn get_unified_cached_envelopes(
    folder: String,
    limit: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<EmailEnvelope>, NimbusError> {
    cache
        .get_unified_envelopes(&folder, limit)
        .map_err(Into::into)
}

#[tauri::command]
fn get_cached_message(
    account_id: String,
    folder: String,
    uid: u32,
    cache: State<'_, Cache>,
) -> Result<Option<Email>, NimbusError> {
    cache
        .get_message(&account_id, &folder, uid)
        .map_err(Into::into)
}

// ── JMAP commands ──────────────────────────────────────────────────

/// Test a JMAP connection by performing session discovery.
///
/// Similar to `test_connection` for IMAP — the setup wizard uses this
/// to verify JMAP credentials before saving the account.
#[tauri::command]
async fn test_jmap_connection(
    jmap_url: String,
    username: String,
    password: String,
) -> Result<String, NimbusError> {
    tracing::info!("Testing JMAP connection to {jmap_url} as {username}");
    JmapClient::test(&jmap_url, &username, &password).await
}

/// Probe whether a server supports JMAP by trying `.well-known/jmap`.
///
/// Returns the JMAP base URL if discovered, or `None` if the server
/// doesn't support JMAP. This is a best-effort probe — it's fine to
/// fall back to IMAP if this fails.
#[tauri::command]
async fn detect_jmap(host: String) -> Result<Option<String>, NimbusError> {
    // Try HTTPS first (standard), then HTTP as fallback.
    for scheme in &["https", "http"] {
        let url = format!("{scheme}://{host}/.well-known/jmap");
        tracing::debug!("Probing JMAP at {url}");

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| NimbusError::Network(format!("HTTP client error: {e}")))?;

        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 401 => {
                // 200 = JMAP available (open session endpoint).
                // 401 = JMAP available but needs auth (common for production servers).
                let base = format!("{scheme}://{host}");
                tracing::info!("JMAP detected at {base}");
                return Ok(Some(base));
            }
            Ok(resp) => {
                tracing::debug!("JMAP probe got HTTP {} — not available", resp.status());
            }
            Err(e) => {
                tracing::debug!("JMAP probe failed: {e}");
            }
        }
    }

    Ok(None)
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
    let cache = ctx.app_handle().state::<Cache>();
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

// ── Search commands (Issue #15) ────────────────────────────────
//
// Two-tier search:
//
//   1. `search_emails`  — instant, against the local FTS5 index.
//                         Covers everything in the cache.
//
//   2. `search_imap_server` — explicit fallback that hits IMAP
//                             `UID SEARCH`. Slower, server-dependent,
//                             only run when the user asks for it
//                             ("Search server too" button).
//
// The cache-first path is the default UX. The fallback is a button
// because (a) it's slow and (b) we don't want to spam the server on
// every keystroke.

/// Run a full-text search against the local mail cache.
///
/// The query is parsed as operator-prefixed syntax (FROM:, TO:,
/// SUBJECT:, etc. — see `nimbus_store::cache::search` for the full
/// grammar). `scope` and
/// `filters` are optional narrowings from the UI — empty values
/// mean "search everything the cache has".
#[tauri::command]
fn search_emails(
    query: String,
    scope: Option<SearchScope>,
    filters: Option<SearchFilters>,
    cache: State<'_, Cache>,
) -> Result<Vec<SearchHit>, NimbusError> {
    let scope = scope.unwrap_or_default();
    let filters = filters.unwrap_or_default();
    cache
        .search_emails(&query, &scope, &filters)
        .map_err(Into::into)
}

/// Server-side IMAP SEARCH fallback. Only JMAP/IMAP — the JMAP
/// client already pulls everything into the cache lazily, so users
/// pointed at a JMAP server get instant results via the local FTS5
/// index and don't need this path.
///
/// Returns envelopes in the same shape as `fetch_envelopes` so the
/// frontend can feed them into the existing mail-list renderer and
/// also upserts them into the local cache so the next search
/// finds them instantly without another round-trip.
#[tauri::command]
async fn search_imap_server(
    account_id: String,
    folder: String,
    query: String,
    limit: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<EmailEnvelope>, NimbusError> {
    let account = load_account(&cache, &account_id)?;
    if uses_jmap(&account) {
        // JMAP cache-first coverage is comprehensive; no separate
        // server-side search path yet. Return empty so the UI
        // silently no-ops the fallback button for JMAP accounts.
        return Ok(Vec::new());
    }

    let criterion = imap_search_criterion(&query);
    if criterion.is_empty() {
        return Ok(Vec::new());
    }

    let mut client = connect_imap(&account).await?;
    let hits = client.search_envelopes(&folder, &criterion, limit).await?;
    let _ = client.logout().await;

    // Warm the cache so the next query is served locally.
    if !hits.is_empty() {
        cache.upsert_envelopes_for_account(&account_id, &hits)?;
    }
    Ok(hits)
}

/// "Load older server-search results" — same shape as
/// `search_imap_server` but takes a `before_uid` cursor so
/// SearchResults can paginate the IMAP search hits past the
/// initial round (#194 follow-up). The frontend tracks the
/// smallest UID it currently has from a previous server-search
/// call and passes it here; we return up to `limit` envelopes
/// matching the same criterion with UID < before_uid.
///
/// JMAP returns empty (same posture as `search_imap_server`,
/// since the JMAP cache-first path covers the user's needs
/// without server pagination).
#[tauri::command]
async fn search_imap_server_older(
    account_id: String,
    folder: String,
    query: String,
    before_uid: u32,
    limit: u32,
    cache: State<'_, Cache>,
) -> Result<Vec<EmailEnvelope>, NimbusError> {
    let account = load_account(&cache, &account_id)?;
    if uses_jmap(&account) {
        return Ok(Vec::new());
    }
    let criterion = imap_search_criterion(&query);
    if criterion.is_empty() {
        return Ok(Vec::new());
    }

    let mut client = connect_imap(&account).await?;
    let hits = client
        .search_envelopes_older(&folder, &criterion, before_uid, limit)
        .await?;
    let _ = client.logout().await;

    if !hits.is_empty() {
        cache.upsert_envelopes_for_account(&account_id, &hits)?;
    }
    Ok(hits)
}

/// Translate a user query into an IMAP SEARCH criterion string.
///
/// We keep this much simpler than the FTS parser — IMAP SEARCH
/// doesn't have rich boolean syntax and most servers only support
/// a small subset of RFC 3501's operators. We emit a conjunction
/// (implicit AND) of `TEXT`/`FROM`/`TO`/`SUBJECT` terms.
///
/// The result is a single string like:
///   `SUBJECT "foo" FROM "alice" TEXT "budget"`
fn imap_search_criterion(query: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut free_text: Vec<String> = Vec::new();

    for token in tokenize_imap_query(query) {
        if let Some((op, value)) = token.split_once(':') {
            let value = value.trim_matches('"');
            if value.is_empty() {
                continue;
            }
            let key = match op.to_ascii_lowercase().as_str() {
                "from" => Some("FROM"),
                "to" => Some("TO"),
                "cc" => Some("CC"),
                "subject" | "title" => Some("SUBJECT"),
                "body" => Some("BODY"),
                _ => None,
            };
            if let Some(k) = key {
                parts.push(format!("{k} \"{}\"", imap_quote(value)));
                continue;
            }
        }
        let cleaned = token.trim_matches('"');
        if !cleaned.is_empty() {
            free_text.push(cleaned.to_string());
        }
    }

    for text in free_text {
        parts.push(format!("TEXT \"{}\"", imap_quote(&text)));
    }

    parts.join(" ")
}

/// Split a query into tokens, keeping quoted phrases intact.
fn tokenize_imap_query(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in input.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                cur.push(c);
            }
            w if w.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Escape `"` and `\` inside an IMAP quoted string (RFC 3501 §4.3).
fn imap_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ── Tray, window lifecycle, and background sync (Issue #16) ────
//
// Three concerns wired together here:
//
//   1. **Tray icon + menu** — always present; gives the user a way
//      back into the app when the window is hidden, plus one-click
//      actions (Check Mail, Compose, Quit).
//   2. **Close-to-tray** — if the user's preference is on, clicking
//      the window's close button hides the window instead of
//      quitting. They quit explicitly via the tray menu.
//   3. **Background sync** — a tokio task polls every configured
//      account's INBOX at a user-set interval. New messages trigger
//      a Tauri event that the frontend turns into an OS toast.
//
// The Rust side deliberately does **not** call the notification
// plugin itself. It emits `new-mail` events with
// `{ account_id, folder, uid, from, subject }` payloads and the
// frontend decides whether (and how) to display them. Rationale:
// one permission check path (in JS), one formatting path, and no
// risk of a background tick racing the OS permission prompt.

#[derive(Debug, Clone, Serialize)]
struct NewMailPayload {
    account_id: String,
    folder: String,
    uid: u32,
    from: String,
    subject: String,
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
struct MailFlagsUpdatedPayload {
    account_id: String,
    folder: String,
}

/// Bring the main window to the front. Called from the tray's
/// left-click handler, the tray menu's "Open Nimbus" item, and the
/// `show_main_window` command.
fn show_main_window(app: &AppHandle) -> Result<(), NimbusError> {
    let win = app
        .get_webview_window("main")
        .ok_or_else(|| NimbusError::Other("main window not found".into()))?;
    // show() may be a no-op if the window is already visible, but
    // unminimize() + set_focus() still make sense in that case.
    let _ = win.show();
    let _ = win.unminimize();
    let _ = win.set_focus();
    Ok(())
}

/// One poll across every configured account's INBOX. Emits `new-mail`
/// for each envelope whose UID is greater than the previously-seen
/// high-water mark, then emits a single `unread-count-updated` with
/// the fresh total. Used by both the periodic loop and the `Check Mail
/// Now` tray/UI action — same code path so manual and automatic
/// refreshes behave identically.
async fn check_mail_now_inner(app: &AppHandle) -> Result<(), NimbusError> {
    let cache = app.state::<Cache>();
    let accounts = account_store::load_accounts(&cache).unwrap_or_default();

    for account in &accounts {
        match poll_folder(account, "INBOX", 20, &cache).await {
            Ok(outcome) => {
                for env in &outcome.new_envelopes {
                    let payload = NewMailPayload {
                        account_id: account.id.clone(),
                        folder: "INBOX".to_string(),
                        uid: env.uid,
                        from: env.from.clone(),
                        subject: env.subject.clone(),
                    };
                    if let Err(e) = app.emit("new-mail", &payload) {
                        tracing::warn!("failed to emit new-mail event: {e}");
                    }
                }
                if !outcome.new_envelopes.is_empty() {
                    tracing::info!(
                        "{}: {} new message(s) in INBOX",
                        account.id,
                        outcome.new_envelopes.len()
                    );
                }
                // #255: when the catch-up flag refresh found a
                // cross-client `\Seen` / `\Flagged` / `\Answered`
                // change, signal the frontend to re-read the cache
                // so the mail list picks it up without a manual
                // refresh.  Skip when nothing changed — silence is
                // the point.
                if outcome.flag_changes > 0 {
                    let payload = MailFlagsUpdatedPayload {
                        account_id: account.id.clone(),
                        folder: "INBOX".to_string(),
                    };
                    if let Err(e) = app.emit("mail-flags-updated", &payload) {
                        tracing::warn!("failed to emit mail-flags-updated event: {e}");
                    }
                }
            }
            Err(e) => {
                // One broken account shouldn't stop us polling the others.
                tracing::warn!("background poll failed for '{}': {e}", account.id);
            }
        }
    }

    // Refresh the tray icon badge, the Windows taskbar overlay, and
    // notify the UI. A failure to read the cache count is non-fatal —
    // the badge stays stale until the next tick.
    refresh_unread_badge(app);

    Ok(())
}

/// Recompute the unread total and apply it everywhere it shows up:
/// the tray icon (badge + tooltip), the Windows taskbar overlay, and
/// the `unread-count-updated` event for the UI.
///
/// Called from three places: the setup hook (paint the initial badge),
/// `check_mail_now_inner` (after polling), and `mark_as_read` (so
/// reading a message visibly drops the count without waiting for the
/// next sync tick).
fn refresh_unread_badge(app: &AppHandle) {
    let total = match app.state::<Cache>().total_unread_count() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("refresh_unread_badge: cache read failed: {e}");
            return;
        }
    };

    if let Some(tray) = app.tray_by_id("nimbus-main") {
        let base = app.state::<TrayBaseIcon>();
        let bitmap = match base.0.lock() {
            Ok(g) => g.clone(),
            Err(e) => {
                tracing::warn!("refresh_unread_badge: tray base lock poisoned: {e}");
                return;
            }
        };
        let badged = badge::render_tray_icon(&bitmap.rgba, bitmap.width, bitmap.height, total);
        if let Err(e) = tray.set_icon(Some(badged)) {
            tracing::warn!("failed to update tray icon: {e}");
        }
        let tip = if total == 0 {
            "Nimbus Mail".to_string()
        } else {
            format!("Nimbus Mail — {total} unread")
        };
        let _ = tray.set_tooltip(Some(&tip));
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
    if let Some(win) = app.get_webview_window("main") {
        let overlay = badge::render_taskbar_overlay(total);
        if let Err(e) = win.set_overlay_icon(overlay) {
            tracing::warn!("failed to set taskbar overlay icon: {e}");
        }
    }

    if let Err(e) = app.emit("unread-count-updated", total) {
        tracing::warn!("failed to emit unread-count-updated: {e}");
    }
    // Issue #115: also push the per-account split so the
    // IconRail can paint a red badge on each account's avatar
    // without doing its own poll.  Soft-fails — the global
    // count above is still informative even if this query
    // bombs.
    match app.state::<Cache>().unread_counts_by_account() {
        Ok(by_acc) => {
            if let Err(e) = app.emit("unread-count-by-account-updated", &by_acc) {
                tracing::warn!("failed to emit unread-count-by-account-updated: {e}");
            }
        }
        Err(e) => tracing::warn!("unread_counts_by_account failed: {e}"),
    }
}

/// Per-account unread INBOX count map, keyed by account id.
/// Used by the IconRail on mount to paint per-avatar badges
/// before the next `unread-count-by-account-updated` event
/// fires (those only land on poll completion).
#[tauri::command]
fn get_unread_counts_by_account(
    cache: State<'_, Cache>,
) -> Result<std::collections::HashMap<String, u32>, NimbusError> {
    cache.unread_counts_by_account().map_err(Into::into)
}

// ── Talk-join reminders (issue #123) ──────────────────────────
//
// Goal: fire a desktop notification ahead of any calendar event
// whose VALARM lead time has just elapsed (issues #123 + #203).
// Lead time is taken from the event's own `VALARM` reminders so
// the user controls timing per-event.  Rides the background sync
// loop's tick, so no extra timers; in-memory dedupe keys off
// `(uid, minutes_before)` so a second tick within the firing
// window doesn't double-toast.
//
// Two settings flags gate the scanner per event:
//   * `meeting_reminders_enabled` — for events that carry a
//     meeting URL (Talk / Zoom / Meet / Teams / Jitsi / …).
//   * `calendar_reminders_enabled` — for events without one.
// Keeping them separate lets users mute one stream without
// silencing the other (e.g. "remind me about meetings but
// don't nag me about every event with an alarm").

/// Lead time in seconds we'll widen the firing window by, on
/// each side of the reminder's exact moment.  Slightly larger
/// than the default 60s tick so a tick that drifts by a few
/// seconds doesn't miss the reminder entirely.
const EVENT_REMINDER_FIRE_TOLERANCE_SECS: i64 = 90;

/// In-memory state for the event-reminder pipeline.
///
/// `fired`: set of `(uid, minutes_before)` pairs we've already
///   pushed a notification for.  Pruned on each scan to drop
///   entries whose event has already started (the reminder is
///   moot once the event is in progress).
/// `dismissed`: UIDs the user explicitly silenced for the rest
///   of the meeting cycle (e.g. after clicking through to join
///   the room — surfaced via the `dismiss_event_reminder` IPC).
/// `snoozes`: UID → "fire again at this time" map populated by
///   the `snooze_event_reminder` IPC when the user picks one of
///   the snooze options on the popup window (#203 follow-up).
///   While a snooze is pending the scanner skips the event's
///   normal VALARM-driven reminders entirely; once `now`
///   crosses the snooze time the scanner fires a synthetic
///   reminder and removes the entry.
#[derive(Default)]
struct EventReminderState {
    fired: Mutex<HashSet<(String, i32)>>,
    dismissed: Mutex<HashSet<String>>,
    snoozes: Mutex<std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>>,
}

/// Pull the first plausible meeting URL out of an event's body
/// text — Nextcloud Talk, Zoom, Teams, Google Meet, Webex, Jitsi,
/// etc.  Any HTTP(S) URL counts; we don't try to be smart about
/// which platform it points at because that ages badly (every
/// quarter brings a new conferencing service).
///
/// Searched fields, in priority order: `URL` (canonical), then
/// `LOCATION` (a common place for join links), then
/// `DESCRIPTION` (where pasted "click to join" links land).
fn extract_meeting_url(event: &CalendarEvent) -> Option<String> {
    fn extract_from(s: &str) -> Option<String> {
        // Walk word by word so the trailing punctuation in
        // pasted plain-text bodies ("…click here: <url>.")
        // doesn't end up baked into the captured URL.
        for token in s.split_whitespace() {
            let url = token.trim_matches(|c: char| {
                c == '<'
                    || c == '>'
                    || c == '"'
                    || c == '\''
                    || c == ','
                    || c == '.'
                    || c == ';'
                    || c == ')'
                    || c == '('
            });
            if url.starts_with("http://") || url.starts_with("https://") {
                return Some(url.to_string());
            }
        }
        None
    }
    let url_field = event.url.as_deref().unwrap_or("");
    let loc_field = event.location.as_deref().unwrap_or("");
    let desc_field = event.description.as_deref().unwrap_or("");
    extract_from(url_field)
        .or_else(|| extract_from(loc_field))
        .or_else(|| extract_from(desc_field))
}

/// Payload pushed to the frontend on every fired reminder.
/// Mirrors the camelCase shape JS expects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventReminderPayload {
    /// Cached event id (`{nc_id}::{cal_path}::{uid}` for masters
    /// / singletons; the `::occ::{epoch}` suffix is included for
    /// expanded recurrence occurrences).  Frontend uses this to
    /// open the event in the editor when the user clicks "Show
    /// event" on the in-app reminder card.
    event_id: String,
    /// Bare VEVENT UID — used for the dismiss-state key so all
    /// occurrences of a recurring series share one dismiss
    /// entry.
    uid: String,
    summary: String,
    /// Event start in UTC RFC 3339 — the JS side localises for
    /// the toast body ("Meeting in 15 min" / "starts at 14:00").
    start: chrono::DateTime<chrono::Utc>,
    /// Event end in UTC RFC 3339.  Surfaced on the in-app card
    /// so the user can see the duration at a glance.
    end: chrono::DateTime<chrono::Utc>,
    /// Free-text location string from the VEVENT (may itself
    /// contain a meeting URL — Nextcloud Calendar puts the Talk
    /// URL here).  `None` when the event has no LOCATION.
    location: Option<String>,
    /// Attendee email list — the in-app card surfaces the first
    /// few + a "+N more" tail.
    attendees: Vec<String>,
    /// First HTTP(S) URL found in URL / LOCATION / DESCRIPTION,
    /// or `None` when the event isn't a meeting at all.  Drives
    /// the per-event gate (`meeting_reminders_enabled` vs
    /// `calendar_reminders_enabled`) and the "Join meeting"
    /// affordance on the in-app card.
    meeting_url: Option<String>,
    /// Lead time the reminder fired at, in minutes.  Lets the
    /// JS side word the toast appropriately ("Now" / "in 5 min"
    /// / "in 1 hour").
    minutes_before: i32,
}

/// Scan upcoming events for ones whose VALARM lead time we've
/// just reached, and emit an `event-reminder` event for each
/// match (gated per-event by the user's two reminder settings —
/// `meeting_reminders_enabled` for events with a meeting URL,
/// `calendar_reminders_enabled` for events without).  Called
/// from the background sync loop; cheap because it reads from
/// the local cache only.
async fn check_event_reminders_inner(app: &AppHandle) -> Result<(), NimbusError> {
    use chrono::Utc;

    let settings = app.state::<SharedSettings>();
    let (meetings_on, calendar_on) = {
        let s = settings.read().await;
        (s.meeting_reminders_enabled, s.calendar_reminders_enabled)
    };
    if !meetings_on && !calendar_on {
        return Ok(());
    }

    // Build the list of calendars whose events should trigger a
    // reminder: every non-hidden, non-muted calendar across every
    // connected NC account.  Mirrors the visibility the user
    // already chose for the agenda grid; muting a calendar there
    // also silences its Talk reminders.
    let cache = app.state::<Cache>();
    let nc_accounts = nextcloud_store::load_accounts(&cache).unwrap_or_default();
    let mut calendar_ids: Vec<String> = Vec::new();
    for acc in &nc_accounts {
        if let Ok(list) = cache.list_calendars(&acc.id) {
            for c in list {
                if !c.hidden && !c.muted {
                    calendar_ids.push(c.id);
                }
            }
        }
    }
    if calendar_ids.is_empty() {
        return Ok(());
    }

    // Window: from now back ~tolerance (so a tick that just
    // crossed the reminder time still catches it) forward 7 days
    // (covers reminders up to "1 week before", the largest
    // preset the editor offers — #236).  An event whose 1-week
    // reminder is approaching has its `start` 7 days from now,
    // so the cache filter must include events that far ahead or
    // the reminder never fires.  Cheap: same per-calendar
    // expansion path the agenda grid already runs, just with a
    // wider date range.
    let now = Utc::now();
    let tolerance = chrono::Duration::seconds(EVENT_REMINDER_FIRE_TOLERANCE_SECS);
    let range_start = now - tolerance;
    let range_end = now + chrono::Duration::days(7) + tolerance;

    let input = match cache.list_events_for_expansion(&calendar_ids, range_start, range_end) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("talk-reminder scan: list_events_for_expansion failed: {e}");
            return Ok(());
        }
    };

    // Re-run the same RRULE expansion the agenda grid uses so
    // the recurring-event case is handled once, here, instead of
    // duplicated.
    let mut overrides_by_master: std::collections::HashMap<&str, Vec<&CalendarEvent>> =
        std::collections::HashMap::new();
    for ov in &input.overrides {
        if let Some(master_id) = ov.id.rsplit_once("::").map(|(prefix, _)| prefix) {
            overrides_by_master.entry(master_id).or_default().push(ov);
        }
    }
    let mut events: Vec<CalendarEvent> = input.singletons;
    for master in &input.masters {
        let ovs = overrides_by_master
            .get(master.id.as_str())
            .cloned()
            .unwrap_or_default();
        events.extend(nimbus_caldav::expand_event(
            master,
            &ovs,
            range_start,
            range_end,
        ));
    }

    let state = app.state::<EventReminderState>();
    {
        // Prune `fired` entries whose event has already started —
        // keeps the set bounded in long-running sessions and
        // ensures a meeting that recurs daily fires its reminder
        // again on the next occurrence.
        let mut fired = state.fired.lock().expect("event-reminder fired mutex");
        let active_uids: HashSet<String> = events
            .iter()
            .filter(|e| e.start > now)
            .map(|e| vevent_uid_from_event_id(&e.id))
            .collect();
        fired.retain(|(uid, _)| active_uids.contains(uid));
    }
    let dismissed_snapshot: HashSet<String> = {
        let d = state
            .dismissed
            .lock()
            .expect("event-reminder dismissed mutex");
        d.clone()
    };
    // Snapshot the snooze map so we can read without holding the
    // lock through the loop — and a separate list of snooze
    // entries to fire & evict at the end of the scan.
    let snoozes_snapshot: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> = {
        let s = state.snoozes.lock().expect("event-reminder snoozes mutex");
        s.clone()
    };
    let mut snoozes_to_evict: Vec<String> = Vec::new();

    for ev in &events {
        // Skip events whose start is far enough in the past that
        // even a 0-min reminder would no longer be inside the
        // per-reminder fire-tolerance window.  Using the same
        // `EVENT_REMINDER_FIRE_TOLERANCE_SECS` constant the
        // per-reminder check uses (vs. the previous hard-coded
        // 1-minute) means the "At event start" preset now has
        // the full tolerance window to fire — without this,
        // a scan tick landing 60–90 s after the event start
        // would silently drop the 0-min reminder even though
        // the per-reminder check would have accepted it.
        if ev.start <= now - chrono::Duration::seconds(EVENT_REMINDER_FIRE_TOLERANCE_SECS) {
            continue;
        }
        let meeting_url = extract_meeting_url(ev);
        // Per-event gate.  Events with a meeting URL ride the
        // `meeting_reminders_enabled` flag; everything else
        // rides `calendar_reminders_enabled`.  Either flag being
        // off silences just that bucket.
        let gate_open = if meeting_url.is_some() {
            meetings_on
        } else {
            calendar_on
        };
        if !gate_open {
            continue;
        }
        let uid = vevent_uid_from_event_id(&ev.id);
        if dismissed_snapshot.contains(&uid) {
            continue;
        }

        // ── Snooze path ───────────────────────────────────────
        // If the user picked "Remind me 5 min before" / etc. on
        // the popup, the dispatch table tells us the next time
        // to fire for this UID.  We *bypass* the VALARM-driven
        // path entirely while a snooze is pending so we don't
        // double-fire from both sources, then re-fire here when
        // `now` crosses the snooze moment.
        if let Some(snooze_until) = snoozes_snapshot.get(&uid) {
            if now < *snooze_until {
                // Still snoozed — skip everything else for this event.
                continue;
            }
            // Snooze elapsed — fire a synthetic reminder with the
            // matching minutes_before label, then evict the entry.
            let minutes_before =
                ((ev.start - now).num_seconds().max(0) / 60).clamp(0, i32::MAX as i64) as i32;
            let payload = EventReminderPayload {
                event_id: ev.id.clone(),
                uid: uid.clone(),
                summary: ev.summary.clone(),
                start: ev.start,
                end: ev.end,
                location: ev.location.clone(),
                attendees: ev.attendees.iter().map(|a| a.email.clone()).collect(),
                meeting_url: meeting_url.clone(),
                minutes_before,
            };
            if let Err(e) = app.emit("event-reminder", &payload) {
                tracing::warn!("failed to emit snoozed event-reminder: {e}");
            } else {
                tracing::info!(
                    "event-reminder fired (post-snooze): uid={} ({} min before)",
                    uid,
                    minutes_before
                );
            }
            snoozes_to_evict.push(uid.clone());
            // Don't also walk the VALARM-driven path for this event
            // on the same scan — the snooze fire stands in for it.
            continue;
        }

        if ev.reminders.is_empty() {
            // No VALARM on the event → user didn't ask for a
            // reminder; respect that.
            continue;
        }

        for reminder in &ev.reminders {
            let minutes = reminder.trigger_minutes_before;
            // Negative `minutes_before` means "after start" — out
            // of scope for a join reminder, skip silently.
            if minutes < 0 {
                continue;
            }
            let fire_at = ev.start - chrono::Duration::minutes(minutes as i64);
            // Fire when `now` is in [fire_at, fire_at + tolerance]:
            // we never look earlier than the requested moment, but
            // do allow a tick's worth of catch-up so a slightly
            // late tick still lands.
            let elapsed = (now - fire_at).num_seconds();
            if !(0..=EVENT_REMINDER_FIRE_TOLERANCE_SECS).contains(&elapsed) {
                continue;
            }

            let key = (uid.clone(), minutes);
            {
                let mut fired = state.fired.lock().expect("event-reminder fired mutex");
                if fired.contains(&key) {
                    continue;
                }
                fired.insert(key);
            }

            let payload = EventReminderPayload {
                event_id: ev.id.clone(),
                uid: uid.clone(),
                summary: ev.summary.clone(),
                start: ev.start,
                end: ev.end,
                location: ev.location.clone(),
                attendees: ev.attendees.iter().map(|a| a.email.clone()).collect(),
                meeting_url: meeting_url.clone(),
                minutes_before: minutes,
            };
            if let Err(e) = app.emit("event-reminder", &payload) {
                tracing::warn!("failed to emit event-reminder: {e}");
            } else {
                tracing::info!(
                    "event-reminder fired: uid={} ({} min before, meeting={})",
                    uid,
                    minutes,
                    meeting_url.is_some()
                );
            }
        }
    }

    // Evict snoozes we just fired so we don't loop on them
    // forever.  Done after the read loop so we never hold the
    // snoozes mutex through the per-event work.
    if !snoozes_to_evict.is_empty() {
        let mut s = state.snoozes.lock().expect("event-reminder snoozes mutex");
        for uid in &snoozes_to_evict {
            s.remove(uid);
        }
    }

    Ok(())
}

/// Recover the bare VEVENT UID from a composite cached id —
/// `{nc_id}::{cal_path}::{uid}` for masters/singletons or
/// `{nc_id}::{cal_path}::{uid}::occ::{epoch}` for expanded
/// occurrences.  The frontend's `dismiss_event_reminder` and the
/// dedupe set both key off the bare UID so all occurrences of
/// the same series share a single dismiss / fire entry.
fn vevent_uid_from_event_id(id: &str) -> String {
    let parts: Vec<&str> = id.split("::").collect();
    if parts.len() >= 3 {
        parts[2].to_string()
    } else {
        id.to_string()
    }
}

/// Suppress further reminders for the given UID until the user
/// reopens the editor or the in-memory state is reset (process
/// restart).  Called from JS when the user clicks Dismiss on
/// the reminder popup or joins a meeting early so we don't
/// pester them mid-event.
#[tauri::command]
fn dismiss_event_reminder(
    uid: String,
    state: State<'_, EventReminderState>,
) -> Result<(), NimbusError> {
    {
        let mut d = state
            .dismissed
            .lock()
            .expect("event-reminder dismissed mutex");
        d.insert(uid.clone());
    }
    {
        let mut f = state.fired.lock().expect("event-reminder fired mutex");
        f.retain(|(u, _)| u != &uid);
    }
    {
        // Snooze and dismiss are mutually exclusive — clear any
        // pending snooze on the same UID so it doesn't fire after
        // the user has already dismissed the event entirely.
        let mut s = state.snoozes.lock().expect("event-reminder snoozes mutex");
        s.remove(&uid);
    }
    Ok(())
}

/// Schedule a re-fire for the given UID at `snooze_until_iso`
/// (RFC 3339 / ISO 8601 in UTC).  Called from JS when the user
/// picks a "Remind me in …" option on the reminder popup.
///
/// While a snooze is pending the scanner skips the event's
/// normal VALARM-driven reminders (so the user doesn't get
/// double-toasted from both sources).  Once `now` crosses the
/// snooze moment the next scan tick fires a synthetic reminder
/// and removes the entry.
#[tauri::command]
fn snooze_event_reminder(
    uid: String,
    snooze_until_iso: String,
    state: State<'_, EventReminderState>,
) -> Result<(), NimbusError> {
    let snooze_until = chrono::DateTime::parse_from_rfc3339(&snooze_until_iso)
        .map_err(|e| {
            NimbusError::Other(format!(
                "snooze_event_reminder: invalid timestamp '{snooze_until_iso}': {e}"
            ))
        })?
        .with_timezone(&chrono::Utc);
    {
        let mut s = state.snoozes.lock().expect("event-reminder snoozes mutex");
        s.insert(uid.clone(), snooze_until);
    }
    {
        // Drop any stale `fired` entry so the scanner is willing
        // to re-fire when the snooze elapses.  Without this the
        // dedupe key `(uid, minutes_before)` would block the
        // post-snooze synthetic reminder.
        let mut f = state.fired.lock().expect("event-reminder fired mutex");
        f.retain(|(u, _)| u != &uid);
    }
    Ok(())
}

/// Launch-time message-body prerender (#178).
///
/// For every configured account, fetch the bodies of the newest INBOX
/// envelopes that don't yet have a cached body.  The user clicking
/// any of those messages then reads from disk instead of paying for
/// an IMAP round-trip — eliminates the "open mail → blank pane →
/// content appears" beat on a fresh launch.
///
/// Bounded to `PRERENDER_LIMIT` per account so a brand-new install
/// (every envelope missing a body) doesn't drown the launch in
/// FETCHes.  Accounts run concurrently; within an account we go
/// sequentially because each `fetch_message_inner` opens its own
/// IMAP connection and we don't want N parallel auths against the
/// same server.
async fn prerender_inboxes_on_launch(app: &AppHandle) {
    /// Ten messages per account is a sweet spot — covers the
    /// usually-visible top of the inbox without ballooning the
    /// launch into a body-sync.  Tuning knob if real-world usage
    /// suggests otherwise.
    const PRERENDER_LIMIT: u32 = 10;

    let cache = app.state::<Cache>();
    let accounts = account_store::load_accounts(&cache).unwrap_or_default();

    let mut handles = Vec::new();
    for account in accounts {
        let app = app.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            let cache = app.state::<Cache>();
            let uids = match cache.get_envelopes_missing_body(&account.id, "INBOX", PRERENDER_LIMIT)
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "prerender: failed to list missing bodies for '{}': {e}",
                        account.id,
                    );
                    return;
                }
            };
            if uids.is_empty() {
                return;
            }
            tracing::info!(
                "prerender: warming {} message body/bodies for '{}'",
                uids.len(),
                account.id,
            );
            for uid in uids {
                if let Err(e) = fetch_message_inner(&account.id, "INBOX", uid, &cache).await {
                    tracing::debug!(
                        "prerender: fetch_message_inner({}, INBOX, {uid}) failed: {e}",
                        account.id,
                    );
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

/// Periodic poll. Re-reads the settings snapshot each tick so the user
/// can toggle sync on/off or change the interval and have it take
/// effect on the next cycle without restarting the loop.
async fn background_sync_loop(app: AppHandle) {
    tracing::info!("background sync loop started");
    loop {
        let (enabled, interval) = {
            let settings = app.state::<SharedSettings>();
            let s = settings.read().await;
            (
                s.background_sync_enabled,
                Duration::from_secs(s.background_sync_interval_secs.max(MIN_SYNC_INTERVAL_SECS)),
            )
        };

        tokio::time::sleep(interval).await;

        if !enabled {
            continue;
        }
        if let Err(e) = check_mail_now_inner(&app).await {
            tracing::warn!("background check_mail_now_inner failed: {e}");
        }
        // Event reminders ride the same tick — the cache is
        // already warm from the mail poll above and the scan is
        // a couple of SQL queries plus an in-memory loop.
        if let Err(e) = check_event_reminders_inner(&app).await {
            tracing::warn!("background check_event_reminders_inner failed: {e}");
        }
        // #276: drain the Outbox.  Walks every queued row across
        // every account and re-attempts the SMTP send.  No-op
        // when the queue is empty (one COUNT(*) check before any
        // network work), so a healthy install pays only the cost
        // of that aggregate per tick.
        drain_outbox_sweep(&app).await;
    }
}

/// Periodic drain pass over `outbox_messages`.  Called from the
/// `background_sync_loop` on every sync tick.  Each row goes
/// through `try_drain_outbox_entry` — same code the
/// `send_email`-spawned task and the manual-retry command use,
/// so a row eventually drains via whichever path completes
/// first.  Done sequentially to keep concurrent SMTP connections
/// to one per account; even a large queue (dozens of rows) is
/// finished well within a sync interval on a healthy network.
async fn drain_outbox_sweep(app: &AppHandle) {
    let cache_state = app.state::<Cache>();
    let rows = match cache_state.list_all_outbox() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("list_all_outbox during drain sweep failed: {e}");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    tracing::info!("outbox drain sweep: {} queued row(s)", rows.len());
    for row in rows {
        try_drain_outbox_entry(app, &cache_state, row.id).await;
    }
}

// ── App-settings commands ──────────────────────────────────────

/// Shared cache for the user's installed font families (#142).
/// Populated once at app startup on a blocking thread so the
/// compose toolbar's font picker reads instantly — re-running
/// font-kit's catalogue walk per dropdown open was visibly
/// laggy on machines with hundreds of fonts.
type SystemFontsCache = Arc<RwLock<Vec<String>>>;

/// Walk the OS font catalogue and return the sorted, de-duped
/// family list.  Pure helper — used by both the startup warmer
/// and a manual refresh path.
fn enumerate_system_fonts() -> Vec<String> {
    let source = font_kit::source::SystemSource::new();
    let families = match source.all_families() {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("font enumeration failed: {e}");
            return Vec::new();
        }
    };
    let mut out: Vec<String> = families
        .into_iter()
        .filter(|f| !f.starts_with('.') && !f.trim().is_empty())
        .collect();
    out.sort_by_key(|a| a.to_lowercase());
    out.dedup();
    out
}

// ── On-disk font cache (#142 follow-up) ───────────────────────
//
// Even with the in-memory cache, a cold launch still pays the
// cost of font-kit's catalogue walk — slow on Linux's first-run
// fontconfig and visible enough that the user complained about
// "first compose" lag.  Persist the result to a JSON file in the
// OS cache dir, signed with a cheap fingerprint of the system
// font directories.  Subsequent launches read the JSON in
// microseconds; we only re-run font-kit when the fingerprint
// changes (i.e. the user actually installed or removed a font).
//
// The fingerprint is a SHA-256 of every font-directory mtime
// found by recursive walk.  Adding or removing a file inside any
// directory updates that directory's mtime on every common
// filesystem, so directory mtimes alone catch both additions and
// removals without us needing to stat every individual font file.

#[derive(serde::Serialize, serde::Deserialize)]
struct FontCacheFile {
    fingerprint: String,
    fonts: Vec<String>,
}

fn font_cache_path() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|d| d.join("nimbus-mail").join("system_fonts.json"))
}

/// Standard system font directories per OS.  Used for the
/// fingerprint walk; font-kit itself looks at more places, but
/// these cover where additions / removals actually happen.
fn font_search_dirs() -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(w) = std::env::var_os("WINDIR") {
            out.push(std::path::PathBuf::from(w).join("Fonts"));
        }
        if let Some(d) = dirs::data_local_dir() {
            out.push(d.join("Microsoft").join("Windows").join("Fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        out.push(std::path::PathBuf::from("/System/Library/Fonts"));
        out.push(std::path::PathBuf::from("/Library/Fonts"));
        if let Some(h) = dirs::home_dir() {
            out.push(h.join("Library").join("Fonts"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        out.push(std::path::PathBuf::from("/usr/share/fonts"));
        out.push(std::path::PathBuf::from("/usr/local/share/fonts"));
        if let Some(h) = dirs::home_dir() {
            out.push(h.join(".fonts"));
            out.push(h.join(".local/share/fonts"));
        }
    }
    out
}

fn collect_dir_mtimes(dir: &std::path::Path, out: &mut Vec<(String, u64)>) {
    let Ok(meta) = std::fs::metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    out.push((dir.to_string_lossy().into_owned(), mtime));
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        if let Ok(m) = entry.metadata()
            && m.is_dir()
        {
            collect_dir_mtimes(&entry.path(), out);
        }
    }
}

fn compute_font_fingerprint() -> String {
    use sha2::{Digest, Sha256};
    let mut pairs: Vec<(String, u64)> = Vec::new();
    for d in font_search_dirs() {
        collect_dir_mtimes(&d, &mut pairs);
    }
    pairs.sort();
    let mut hasher = Sha256::new();
    for (p, m) in &pairs {
        hasher.update(p.as_bytes());
        hasher.update(b"|");
        hasher.update(m.to_string().as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

fn load_font_cache_file() -> Option<FontCacheFile> {
    let path = font_cache_path()?;
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_font_cache_file(file: &FontCacheFile) {
    let Some(path) = font_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(file) {
        let _ = std::fs::write(&path, bytes);
    }
}

// ── FIDO unlock (#164, Phase 1A) ──────────────────────────────
//
// These commands manage the wraps inside the keychain envelope.
// They don't yet replace the plain-mode startup path — registering
// keys is observable via the Settings UI, and the unlock-at-boot
// flow lands as a separate phase once the wrap/unwrap loop is
// hardware-verified.

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FidoCredentialView {
    /// `"fido_prf"` or `"passphrase"`.
    kind: String,
    credential_id: String,
    label: String,
    salt: String,
    created_at: i64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FidoStatusView {
    /// Always Some in plain / hybrid mode, None once the keychain
    /// is in FIDO-only mode (Phase 1B+).
    has_plain_key: bool,
    /// How many credentials the user has registered.
    credentials: Vec<FidoCredentialView>,
}

/// Snapshot of the keychain envelope.  Used by Settings to render
/// the "Hardware authentication" panel and (later) by the boot
/// path to decide whether to require an unlock before opening the
/// cache.
#[tauri::command]
fn fido_status() -> Result<FidoStatusView, NimbusError> {
    let env = nimbus_store::cache::key::load_envelope()?;
    Ok(FidoStatusView {
        has_plain_key: env.plain_key.is_some(),
        credentials: env
            .wraps
            .into_iter()
            .map(|w| FidoCredentialView {
                kind: match w.kind {
                    nimbus_store::fido::WrapKind::FidoPrf => "fido_prf".to_string(),
                    nimbus_store::fido::WrapKind::Passphrase => "passphrase".to_string(),
                },
                credential_id: w.credential_id,
                label: w.label,
                salt: w.salt,
                created_at: w.created_at,
            })
            .collect(),
    })
}

/// Generate a fresh PRF salt for a new enrollment.  The frontend
/// supplies it as the `prf.eval.first` input to `navigator.
/// credentials.create` so the authenticator returns the matching
/// PRF output.
#[tauri::command]
fn fido_generate_salt() -> Result<String, NimbusError> {
    let salt = nimbus_store::fido::generate_salt()?;
    Ok(nimbus_store::fido::encode_b64(&salt))
}

/// Wrap the current master key under a freshly-registered FIDO
/// credential's PRF output.  Frontend has already called
/// WebAuthn `credentials.create` with the salt from
/// `fido_generate_salt`, received the credential id and the PRF
/// bytes back, and forwards them here for storage.
#[tauri::command]
fn fido_enroll(
    credential_id_b64: String,
    salt_b64: String,
    prf_output_b64: String,
    label: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    use nimbus_store::fido;
    let env = nimbus_store::cache::key::load_envelope()?;
    // Same fallback as `fido_enroll_passphrase`: prefer the
    // envelope's plain key, fall back to the in-memory copy
    // when FIDO-only mode has cleared plain_key.
    let plain_hex = match env.plain_key.as_deref() {
        Some(hex) => hex.to_string(),
        None => cache.master_key_hex().ok_or_else(|| {
            NimbusError::Auth(
                "Cannot enroll a credential while the database is locked — unlock first".into(),
            )
        })?,
    };
    let master_key = hex::decode(&plain_hex)
        .map_err(|e| NimbusError::Storage(format!("master key hex decode: {e}")))?;
    let credential_id = fido::decode_b64(&credential_id_b64)?;
    let salt = fido::decode_b64(&salt_b64)?;
    let prf_output = fido::decode_b64(&prf_output_b64)?;
    let wrap = fido::wrap_master_key(
        fido::WrapKind::FidoPrf,
        &master_key,
        &prf_output,
        &credential_id,
        &salt,
        label,
    )?;
    nimbus_store::cache::key::add_wrap(wrap)?;
    Ok(())
}

/// Wrap the current master key under a passphrase-derived AES key
/// (PBKDF2-HMAC-SHA-256, 720 000 iters).  Doubles as recovery
/// passphrase for Phase 1B and as the test path on platforms
/// where WebAuthn PRF isn't reachable yet (Linux WebKitGTK <
/// 2.46).  Salt + synthetic credential id are server-side
/// generated so the frontend never produces them.
#[tauri::command]
fn fido_enroll_passphrase(
    passphrase: String,
    label: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    use nimbus_store::fido::{self, WrapKind};
    if passphrase.trim().is_empty() {
        return Err(NimbusError::Other("passphrase must not be empty".into()));
    }
    let mut env = nimbus_store::cache::key::load_envelope()?;
    // Prefer the keychain envelope's plain key (pre-FIDO-only),
    // fall back to the in-memory copy that `unlock_with_*` stashes
    // on the Cache.  The fallback is what makes "Change passphrase"
    // work after the user has flipped Key Encryption on — by that
    // point envelope.plain_key is None and we'd otherwise refuse.
    let plain_hex = match env.plain_key.as_deref() {
        Some(hex) => hex.to_string(),
        None => cache.master_key_hex().ok_or_else(|| {
            NimbusError::Auth(
                "Cannot enroll a passphrase while the database is locked — unlock first".into(),
            )
        })?,
    };
    let master_key = hex::decode(&plain_hex)
        .map_err(|e| NimbusError::Storage(format!("master key hex decode: {e}")))?;
    let salt = fido::generate_salt()?;
    let id = fido::generate_passphrase_id()?;
    let aes_key = fido::derive_passphrase_key(&passphrase, &salt)?;
    let wrap = fido::wrap_master_key(
        WrapKind::Passphrase,
        &master_key,
        &aes_key,
        &id,
        &salt,
        label,
    )?;
    // Single-passphrase invariant: the recovery passphrase is a
    // role, not a per-device entry.  Drop any existing passphrase
    // wrap before adding the new one so re-enrolling cleanly
    // replaces the old one (and so add_wrap's credential-id
    // dedup never lets two passphrase wraps coexist with
    // different ids).
    env.wraps.retain(|w| w.kind != WrapKind::Passphrase);
    env.wraps.push(wrap);
    nimbus_store::cache::key::save_envelope(&env)?;
    Ok(())
}

/// Test-only: verify a passphrase wraps unlock the master key.
/// Phase 1B will call this from the lock screen when the user
/// chooses passphrase unlock; today it lets users sanity-check
/// their passphrase entry on Linux without restructuring boot.
/// Returns `true` on success, `false` on a wrong passphrase /
/// no matching wrap, error on storage / crypto failure.
#[tauri::command]
fn fido_verify_passphrase(passphrase: String) -> Result<bool, NimbusError> {
    use nimbus_store::fido::{self, WrapKind};
    let env = nimbus_store::cache::key::load_envelope()?;
    for wrap in &env.wraps {
        if wrap.kind != WrapKind::Passphrase {
            continue;
        }
        let salt = fido::decode_b64(&wrap.salt)?;
        let aes_key = fido::derive_passphrase_key(&passphrase, &salt)?;
        if fido::unwrap_master_key(wrap, &aes_key).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Mirror of `fido_verify_passphrase` for FIDO PRF wraps.  The
/// frontend has already run WebAuthn `credentials.get` against
/// the credential's stored salt and forwards the PRF output
/// here.  Phase 1B's lock screen will use this; today it lets
/// you sanity-check that a registered hardware key still works.
#[tauri::command]
fn fido_verify_prf(credential_id_b64: String, prf_output_b64: String) -> Result<bool, NimbusError> {
    use nimbus_store::fido::{self, WrapKind};
    let env = nimbus_store::cache::key::load_envelope()?;
    let prf = fido::decode_b64(&prf_output_b64)?;
    for wrap in &env.wraps {
        if wrap.kind != WrapKind::FidoPrf {
            continue;
        }
        if wrap.credential_id != credential_id_b64 {
            continue;
        }
        if fido::unwrap_master_key(wrap, &prf).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Remove a registered credential.  Refuses to drop the last wrap
/// when the keychain is in FIDO-only mode (would orphan the
/// encrypted DB).
#[tauri::command]
fn fido_remove(credential_id_b64: String) -> Result<(), NimbusError> {
    let env = nimbus_store::cache::key::load_envelope()?;
    if env.plain_key.is_none() && env.wraps.len() <= 1 {
        return Err(NimbusError::Other(
            "Cannot remove the last hardware key while FIDO-only mode is active".into(),
        ));
    }
    nimbus_store::cache::key::remove_wrap(&credential_id_b64)?;
    Ok(())
}

// ── Database lock + unlock (#164 Phase 1B) ────────────────────

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DatabaseStatusView {
    /// True when no plain key is in the envelope and the cache
    /// pool isn't open yet — the lock screen should be shown.
    locked: bool,
    /// True when the keychain envelope has zero registered methods
    /// and zero plain key — the user has wiped everything;
    /// app needs to recreate from scratch.
    needs_setup: bool,
    /// One entry per registered unlock method (FIDO PRF or
    /// passphrase), used by the lock screen to render a picker.
    methods: Vec<FidoCredentialView>,
    /// Remaining unlock attempts before wipe-on-failure fires.
    /// `None` when the policy is off or has no limit set —
    /// the lock screen renders "X tries remaining" only when this
    /// is `Some(_)`.
    attempts_remaining: Option<u32>,
}

/// Snapshot used by `App.svelte` on mount to decide whether to
/// route the user to the lock screen or straight into the inbox.
#[tauri::command]
fn database_status(cache: State<'_, Cache>) -> Result<DatabaseStatusView, NimbusError> {
    let env = nimbus_store::cache::key::load_envelope()?;
    let locked = cache.is_locked();
    let attempts_remaining = match (env.wipe_on_failure, env.max_unlock_attempts) {
        (true, Some(max)) if max > 0 => Some(max.saturating_sub(env.failed_attempts)),
        _ => None,
    };
    Ok(DatabaseStatusView {
        locked,
        needs_setup: env.plain_key.is_none() && env.wraps.is_empty(),
        methods: env
            .wraps
            .into_iter()
            .map(|w| FidoCredentialView {
                kind: match w.kind {
                    nimbus_store::fido::WrapKind::FidoPrf => "fido_prf".to_string(),
                    nimbus_store::fido::WrapKind::Passphrase => "passphrase".to_string(),
                },
                credential_id: w.credential_id,
                label: w.label,
                salt: w.salt,
                created_at: w.created_at,
            })
            .collect(),
        attempts_remaining,
    })
}

/// Wipe the cache file and clear the keychain envelope.
/// Triggered when the user exhausts their unlock budget OR
/// when the envelope's integrity MAC fails.
fn perform_wipe(cache: &Cache) {
    if let Err(e) = cache.wipe_on_disk() {
        tracing::error!("wipe_on_disk failed: {e}");
    }
    let cleared = nimbus_store::fido::KeychainEnvelope {
        version: 1,
        plain_key: None,
        wraps: Vec::new(),
        wipe_on_failure: false,
        max_unlock_attempts: None,
        failed_attempts: 0,
        integrity_mac: None,
    };
    if let Err(e) = nimbus_store::cache::key::save_envelope(&cleared) {
        tracing::error!("clearing envelope after wipe failed: {e}");
    }
}

/// Bump the persisted failure counter and, if the user has
/// opted into the wipe-on-failure policy, blow away the cache
/// once the configured retry budget is exhausted.  The counter
/// lives in the keychain envelope (not just process memory) so
/// kill+relaunch can't reset the budget.  An invalid envelope
/// MAC trips the wipe immediately on the next failure regardless
/// of where the persisted counter sat.
fn note_unlock_failure(cache: &Cache, label: &str) -> NimbusError {
    let mut env = match nimbus_store::cache::key::load_envelope() {
        Ok(e) => e,
        Err(e) => return e,
    };
    let tampered = nimbus_store::cache::key::envelope_tampered(&env);
    if tampered {
        tracing::warn!("Keychain envelope MAC mismatch — treating this attempt as terminal.");
    }
    env.failed_attempts = env.failed_attempts.saturating_add(1);
    let attempts = env.failed_attempts;
    if let Err(e) = nimbus_store::cache::key::save_envelope(&env) {
        tracing::warn!("could not persist failure counter: {e}");
    }
    if env.wipe_on_failure || tampered {
        let max = env.max_unlock_attempts.unwrap_or(0);
        let trip = tampered || (max > 0 && attempts >= max);
        if trip {
            if tampered {
                tracing::warn!("Wipe fired due to envelope tampering.");
            } else {
                tracing::warn!(
                    "Wipe-on-failure policy fired: {attempts} consecutive failed unlock attempts (limit {max})."
                );
            }
            perform_wipe(cache);
            return NimbusError::Auth(if tampered {
                "Keychain envelope was modified outside Nimbus. The encrypted cache has been wiped."
                    .to_string()
            } else {
                format!(
                    "Too many failed attempts ({attempts}/{max}). The encrypted cache has been wiped."
                )
            });
        }
    }
    NimbusError::Auth(format!("incorrect {label}"))
}

/// Reset the persisted failure counter on a successful unlock.
fn note_unlock_success() {
    let Ok(mut env) = nimbus_store::cache::key::load_envelope() else {
        return;
    };
    if env.failed_attempts == 0 {
        return;
    }
    env.failed_attempts = 0;
    if let Err(e) = nimbus_store::cache::key::save_envelope(&env) {
        tracing::warn!("could not reset failure counter: {e}");
    }
}

/// Unlock the cache from a passphrase.  Tries every passphrase
/// wrap in the envelope, returns the first match.
#[tauri::command]
fn unlock_with_passphrase(passphrase: String, cache: State<'_, Cache>) -> Result<(), NimbusError> {
    use nimbus_store::fido::{self, WrapKind};
    let env = nimbus_store::cache::key::load_envelope()?;
    for wrap in &env.wraps {
        if wrap.kind != WrapKind::Passphrase {
            continue;
        }
        let salt = fido::decode_b64(&wrap.salt)?;
        let aes_key = fido::derive_passphrase_key(&passphrase, &salt)?;
        if let Ok(master) = fido::unwrap_master_key(wrap, &aes_key) {
            let hex = hex::encode(&master);
            cache
                .unlock_with_master_key(hex)
                .map_err(NimbusError::from)?;
            note_unlock_success();
            return Ok(());
        }
    }
    Err(note_unlock_failure(&cache, "passphrase"))
}

/// Unlock the cache from a FIDO PRF assertion.  Frontend has
/// already run WebAuthn `credentials.get` against the
/// credential's stored salt and forwards the resulting PRF
/// output here.
#[tauri::command]
fn unlock_with_prf(
    credential_id_b64: String,
    prf_output_b64: String,
    cache: State<'_, Cache>,
) -> Result<(), NimbusError> {
    use nimbus_store::fido::{self, WrapKind};
    let env = nimbus_store::cache::key::load_envelope()?;
    let prf = fido::decode_b64(&prf_output_b64)?;
    for wrap in &env.wraps {
        if wrap.kind != WrapKind::FidoPrf || wrap.credential_id != credential_id_b64 {
            continue;
        }
        let master = match fido::unwrap_master_key(wrap, &prf) {
            Ok(m) => m,
            Err(_) => return Err(note_unlock_failure(&cache, "hardware key PRF output")),
        };
        let hex = hex::encode(&master);
        cache
            .unlock_with_master_key(hex)
            .map_err(NimbusError::from)?;
        note_unlock_success();
        return Ok(());
    }
    Err(NimbusError::Auth(
        "no registered hardware key matches that credential".into(),
    ))
}

/// Switch the cache into FIDO-only mode: drop the plain master
/// key from the keychain envelope so future cold launches MUST
/// authenticate with one of the registered methods.  Refuses
/// unless the user has at least one passphrase OR ≥ 2 hardware
/// keys registered — without a recovery option we'd lock them
/// out permanently the first time a YubiKey gets lost.
#[tauri::command]
fn enable_fido_only_mode() -> Result<(), NimbusError> {
    use nimbus_store::fido::WrapKind;
    let mut env = nimbus_store::cache::key::load_envelope()?;
    if env.plain_key.is_none() {
        return Ok(()); // already FIDO-only — idempotent.
    }
    let passphrase_count = env
        .wraps
        .iter()
        .filter(|w| w.kind == WrapKind::Passphrase)
        .count();
    let fido_count = env
        .wraps
        .iter()
        .filter(|w| w.kind == WrapKind::FidoPrf)
        .count();
    if passphrase_count == 0 && fido_count < 2 {
        return Err(NimbusError::Other(
            "Register at least one passphrase OR two hardware keys before enabling FIDO-only mode \
             — otherwise losing a single key would lock the cache permanently."
                .into(),
        ));
    }
    env.plain_key = None;
    nimbus_store::cache::key::save_envelope(&env)?;
    Ok(())
}

/// Snapshot of the wipe-on-failure policy stored in the
/// keychain envelope.  `enabled = false` means unlimited
/// retries.  `max_attempts = None` means the same — the toggle
/// can be on but with no number set; we treat that as
/// effectively off until a number is provided.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WipePolicyView {
    enabled: bool,
    max_attempts: Option<u32>,
}

#[tauri::command]
fn get_wipe_policy() -> Result<WipePolicyView, NimbusError> {
    let env = nimbus_store::cache::key::load_envelope()?;
    Ok(WipePolicyView {
        enabled: env.wipe_on_failure,
        max_attempts: env.max_unlock_attempts,
    })
}

#[tauri::command]
fn set_wipe_policy(policy: WipePolicyView) -> Result<(), NimbusError> {
    let mut env = nimbus_store::cache::key::load_envelope()?;
    env.wipe_on_failure = policy.enabled;
    env.max_unlock_attempts = if policy.enabled {
        policy.max_attempts.filter(|n| *n > 0)
    } else {
        None
    };
    nimbus_store::cache::key::save_envelope(&env)?;
    Ok(())
}

/// Reverse of `enable_fido_only_mode` — re-store the plain
/// master key in the envelope so the next launch opens the
/// cache without prompting.  Only callable while the cache is
/// already unlocked (we need the in-memory key).
#[tauri::command]
fn disable_fido_only_mode(cache: State<'_, Cache>) -> Result<(), NimbusError> {
    if cache.is_locked() {
        return Err(NimbusError::Auth(
            "Database must be unlocked before FIDO-only mode can be disabled".into(),
        ));
    }
    let key_hex = cache.master_key_hex().ok_or_else(|| {
        NimbusError::Auth(
            "Master key isn't available in memory — unlock the database again before disabling key encryption".into(),
        )
    })?;
    let mut env = nimbus_store::cache::key::load_envelope()?;
    if env.plain_key.is_some() {
        return Ok(()); // already plain — idempotent.
    }
    env.plain_key = Some(key_hex);
    nimbus_store::cache::key::save_envelope(&env)?;
    Ok(())
}

/// Return the cached font list to the frontend.  Reads from
/// the shared `SystemFontsCache` populated at startup; if the
/// cache is somehow empty (startup warmer failed or hasn't run
/// yet), runs the enumeration once on a blocking thread and
/// memoises the result before returning.
#[tauri::command]
async fn list_system_fonts(cache: State<'_, SystemFontsCache>) -> Result<Vec<String>, NimbusError> {
    {
        let snap = cache.read().await;
        if !snap.is_empty() {
            return Ok(snap.clone());
        }
    }
    // Cold path: warm the cache synchronously this once.
    let fonts = tokio::task::spawn_blocking(enumerate_system_fonts)
        .await
        .map_err(|e| NimbusError::Other(format!("font enumeration join: {e}")))?;
    *cache.write().await = fonts.clone();
    Ok(fonts)
}

#[tauri::command]
async fn get_app_settings(settings: State<'_, SharedSettings>) -> Result<AppSettings, NimbusError> {
    Ok(settings.read().await.clone())
}

#[tauri::command]
async fn update_app_settings(
    new_settings: AppSettings,
    settings: State<'_, SharedSettings>,
    notify: State<'_, SettingsSyncNotify>,
) -> Result<(), NimbusError> {
    app_settings::save_settings(&new_settings)?;
    *settings.write().await = new_settings;
    notify.0.notify_one();
    Ok(())
}

// ── Settings backup & sync (#168) ──────────────────────────────
//
// Lets the user save every preference (app-wide settings, account
// metadata, folder→emoji mappings, signature, locale, theme, …)
// to either a local `settings.json` or to a connected Nextcloud
// under `/Nimbus Mail/settings/settings.json`.  Restoring is the
// reverse: pick a file, or — on first NC connect — accept the
// "found a backup, restore?" prompt.
//
// Architecture
//
//   • Bundle = { version, exported_at, app_settings, accounts,
//                local_storage }.  Schema-versioned JSON; secrets
//                (passwords, FIDO wraps, master key) deliberately
//                excluded.  See `nimbus_store::settings_bundle`.
//
//   • NC sync runs in a background task.  Frontend calls
//     `notify_settings_changed(local_storage)` after any UI
//     mutation; the worker debounces 2 s, then PUTs the bundle
//     to NC.  Failure flips `settings_sync::SettingsSyncState
//     ::pending` to true so the next opportunity (next change OR
//     the periodic 5-min retry) takes another shot.
//
//   • The worker is started once from `main()` after the cache
//     unlocks (it needs `Cache` access for accounts).  On launch
//     it consults `pending` from disk; if true, it attempts a
//     push immediately so a quit-while-offline still recovers.
//
//   • Sync target (`target_nc_id`) is stored *outside* the
//     bundle — it's a per-machine choice.  Restoring on a new
//     device shouldn't silently start syncing back to the old
//     server.

/// Path inside a user's Nextcloud where the settings bundle
/// lives.  Sits under the existing `/Nimbus Mail` root so it
/// shares a folder with the temp area used by Office viewer.
const NIMBUS_SETTINGS_DIR: &str = "/Nimbus Mail/settings";
const NIMBUS_SETTINGS_FILE: &str = "/Nimbus Mail/settings/settings.json";

/// Latest `localStorage` snapshot the frontend has shared with
/// us.  The auto-sync worker reads from here so it can assemble
/// a complete bundle without an additional IPC round-trip.
type SharedLocalStorage = Arc<RwLock<std::collections::HashMap<String, String>>>;

/// Notify channel used to wake the auto-sync worker.  Each
/// `notify_one()` call coalesces with any already-pending wakeup,
/// so a burst of settings changes still results in a single push
/// once the debounce window expires.
struct SettingsSyncNotify(Arc<tokio::sync::Notify>);

/// Return the live `AppSettings` + accounts + the frontend's
/// supplied `local_storage` map as one JSON-serialisable bundle.
/// This is the single entry point the frontend uses for both
/// "Download settings" (writes the JSON via `dialog.save` on the
/// frontend) and the manual "Sync now" path.
#[tauri::command]
async fn build_settings_bundle(
    local_storage: std::collections::HashMap<String, String>,
    cache: State<'_, Cache>,
) -> Result<String, NimbusError> {
    let bundle = settings_bundle::build_bundle(&cache, local_storage)?;
    settings_bundle::serialise(&bundle)
}

/// Apply a previously-exported bundle.  Replaces `app_settings`,
/// upserts each account by id, and returns the bundle's
/// `local_storage` map so the frontend can write each key back
/// into its own `localStorage`.  The frontend reloads its UI
/// after this returns — most preferences only re-apply on the
/// next render pass.
#[tauri::command]
async fn apply_settings_bundle(
    json: String,
    cache: State<'_, Cache>,
    settings: State<'_, SharedSettings>,
) -> Result<std::collections::HashMap<String, String>, NimbusError> {
    let bundle = settings_bundle::parse(&json)?;
    let new_app_settings = bundle.app_settings.clone();
    let local_storage = settings_bundle::apply(&cache, bundle)?;
    *settings.write().await = new_app_settings;
    Ok(local_storage)
}

/// Frontend-facing view of `settings_sync::SettingsSyncState`.
/// camelCase for the JSON IPC convention used elsewhere in the
/// file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsSyncStateView {
    target_nc_id: Option<String>,
    pending: bool,
}

#[tauri::command]
fn get_settings_sync_state() -> Result<SettingsSyncStateView, NimbusError> {
    let state = settings_sync::load_state()?;
    Ok(SettingsSyncStateView {
        target_nc_id: state.target_nc_id,
        pending: state.pending,
    })
}

/// Pick (or clear) the connected Nextcloud account that recovery
/// pushes go to.  Passing `None` turns the feature off.  Setting
/// it kicks off a sync immediately so the chosen NC has a fresh
/// copy without waiting for the next settings change.
#[tauri::command]
async fn set_settings_sync_target(
    target_nc_id: Option<String>,
    notify: State<'_, SettingsSyncNotify>,
) -> Result<(), NimbusError> {
    let mut state = settings_sync::load_state()?;
    if state.target_nc_id == target_nc_id {
        return Ok(());
    }
    state.target_nc_id = target_nc_id;
    // Flipping the target counts as a "settings changed" event —
    // the new NC needs a fresh push so a future restore actually
    // finds something there.
    state.pending = state.target_nc_id.is_some();
    settings_sync::save_state(&state)?;
    notify.0.notify_one();
    Ok(())
}

/// Frontend-side hook: call after any settings mutation that the
/// user could plausibly want backed up.  Stores the latest
/// `localStorage` snapshot in shared state and pings the auto-
/// sync worker, which debounces and pushes to NC if a target is
/// set.  No-ops cleanly when sync is off.
#[tauri::command]
async fn notify_settings_changed(
    local_storage: std::collections::HashMap<String, String>,
    storage: State<'_, SharedLocalStorage>,
    notify: State<'_, SettingsSyncNotify>,
) -> Result<(), NimbusError> {
    *storage.write().await = local_storage;
    notify.0.notify_one();
    Ok(())
}

/// Probe a connected NC for a previously-uploaded settings
/// bundle.  Returns `None` if no bundle exists at the canonical
/// path, the parsed bundle's `exported_at` if one is found.
/// Used by the "found a backup, restore?" prompt the frontend
/// shows on a fresh NC connect.
#[tauri::command]
async fn nc_probe_settings_bundle(
    nc_id: String,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    match nimbus_nextcloud::download_file(
        &account.server_url,
        &account.username,
        &app_password,
        NIMBUS_SETTINGS_FILE,
        &account.trusted_certs,
    )
    .await
    {
        Ok(bytes) => {
            let json = String::from_utf8(bytes).map_err(|e| {
                NimbusError::Storage(format!("settings bundle on NC is not UTF-8: {e}"))
            })?;
            let bundle = settings_bundle::parse(&json)?;
            Ok(Some(bundle.exported_at))
        }
        // 404 = no backup, that's the normal first-time path.
        // We map it through to None so the UI can stay quiet
        // instead of surfacing an error toast.
        Err(NimbusError::Nextcloud(msg)) if msg.contains("not found") => Ok(None),
        Err(e) => Err(e),
    }
}

/// Download the bundle from NC and apply it.  Used by the
/// "restore on first NC connect" prompt and by a manual "Restore
/// from Nextcloud" button on the Backup & Sync settings page.
#[tauri::command]
async fn nc_restore_settings_bundle(
    nc_id: String,
    cache: State<'_, Cache>,
    settings: State<'_, SharedSettings>,
) -> Result<std::collections::HashMap<String, String>, NimbusError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let bytes = nimbus_nextcloud::download_file(
        &account.server_url,
        &account.username,
        &app_password,
        NIMBUS_SETTINGS_FILE,
        &account.trusted_certs,
    )
    .await?;
    let json = String::from_utf8(bytes)
        .map_err(|e| NimbusError::Storage(format!("settings bundle on NC is not UTF-8: {e}")))?;
    let bundle = settings_bundle::parse(&json)?;
    let new_app_settings = bundle.app_settings.clone();
    let local_storage = settings_bundle::apply(&cache, bundle)?;
    *settings.write().await = new_app_settings;
    Ok(local_storage)
}

/// One push attempt.  Best-effort folder creation, then PUT.
/// Folder creates are intentionally swallowed because
/// `create_directory` returns `NimbusError::Nextcloud` for the
/// idempotent "folder already exists" case — it's not actually
/// an error from our perspective.
async fn push_settings_to_nc(
    cache: &Cache,
    local_storage: std::collections::HashMap<String, String>,
    nc_id: &str,
) -> Result<(), NimbusError> {
    let bundle = settings_bundle::build_bundle(cache, local_storage)?;
    let json = settings_bundle::serialise(&bundle)?;

    let account = nextcloud_store::load_accounts(cache)?
        .into_iter()
        .find(|a| a.id == nc_id)
        .ok_or_else(|| NimbusError::Other(format!("Nextcloud account '{nc_id}' not found")))?;
    let app_password = credentials::get_nextcloud_password(nc_id)?;

    // Idempotent folder creates.  The Office viewer code already
    // ensures `/Nimbus Mail` for the temp area, but a user who
    // hasn't opened any Office attachments won't have triggered
    // that path yet — we make sure both rungs of the hierarchy
    // exist before the PUT.
    for dir in [NIMBUS_TEMP_ROOT, NIMBUS_SETTINGS_DIR] {
        if let Err(e) = nimbus_nextcloud::create_directory(
            &account.server_url,
            &account.username,
            &app_password,
            dir,
            &account.trusted_certs,
        )
        .await
        {
            // 405 / "already exists" is the happy path; only the
            // network/auth/quota classes need to bubble up.
            let msg = e.to_string();
            if !msg.contains("already") && !msg.contains("405") && !msg.contains("HTTP 405") {
                return Err(e);
            }
        }
    }

    nimbus_nextcloud::upload_file(
        &account.server_url,
        &account.username,
        &app_password,
        NIMBUS_SETTINGS_FILE,
        json.into_bytes(),
        Some("application/json"),
        &account.trusted_certs,
    )
    .await?;
    Ok(())
}

/// Auto-sync worker.  Wakes on either a `notify_one()` from a
/// settings-changed event or a 5-minute periodic tick (the retry
/// path for "user changed a setting while offline and never
/// changed another"), and pushes the bundle to the configured NC
/// account if one is set.  Failures keep `pending=true` so the
/// next opportunity tries again.
async fn settings_sync_worker(
    cache: Cache,
    local_storage: SharedLocalStorage,
    notify: Arc<tokio::sync::Notify>,
) {
    use tokio::time::{Duration, MissedTickBehavior, interval, sleep};

    let mut retry_tick = interval(Duration::from_secs(300));
    retry_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The first `tick()` returns immediately — burn it so the
    // periodic path doesn't fire on launch.  The launch-time
    // recovery happens via the explicit `notify_one()` call from
    // `main()` instead.
    retry_tick.tick().await;

    loop {
        tokio::select! {
            _ = notify.notified() => {
                // Debounce: a burst of changes (e.g. dragging
                // the UI scale slider) coalesces into one push.
                sleep(Duration::from_secs(2)).await;
            }
            _ = retry_tick.tick() => {
                // Periodic retry — only meaningful if we have
                // something to flush, so peek the disk state.
                let state = settings_sync::load_state().unwrap_or_default();
                if !state.pending || state.target_nc_id.is_none() {
                    continue;
                }
            }
        }

        // Read the disk state fresh; the user may have flipped
        // the toggle off between the wake and now.
        let state = match settings_sync::load_state() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("settings_sync load_state failed: {e}");
                continue;
            }
        };
        let Some(target) = state.target_nc_id.clone() else {
            // Sync turned off — clear any stale pending flag so
            // a re-enable doesn't immediately fire a stale push.
            if state.pending {
                let _ = settings_sync::save_state(&settings_sync::SettingsSyncState {
                    target_nc_id: None,
                    pending: false,
                });
            }
            continue;
        };

        let snapshot = local_storage.read().await.clone();
        match push_settings_to_nc(&cache, snapshot, &target).await {
            Ok(()) => {
                tracing::info!("Settings bundle synced to Nextcloud '{target}'");
                if state.pending {
                    let _ = settings_sync::save_state(&settings_sync::SettingsSyncState {
                        target_nc_id: state.target_nc_id,
                        pending: false,
                    });
                }
            }
            Err(e) => {
                // Silent in the UI; warn-level in the log so a
                // developer chasing "why isn't my NC backup
                // updating" can see what went wrong.
                tracing::warn!("Settings sync to '{target}' failed (will retry later): {e}");
                if !state.pending {
                    let _ = settings_sync::save_state(&settings_sync::SettingsSyncState {
                        target_nc_id: state.target_nc_id,
                        pending: true,
                    });
                }
            }
        }
    }
}

#[tauri::command]
async fn check_mail_now(app: AppHandle) -> Result<(), NimbusError> {
    check_mail_now_inner(&app).await
}

// ── URLhaus link safety (#165) ─────────────────────────────────
//
// Local snapshot of abuse.ch's URLhaus "online malicious URLs"
// CSV.  Refreshed every hour by a background task; lookups go
// against the encrypted SQLite cache (so the URL list inherits
// the same at-rest protection as the user's mail).
//
// Frontend behaviour:
//   - On message open, MailView walks every <a href> in the
//     rendered body, batches the URLs into one `check_urls`
//     IPC, and renders a green "Safe" / red "Unsafe" pill next
//     to each link.
//   - Click on an Unsafe link is intercepted: a confirm modal
//     offers "Delete mail" (move to Trash) or "Open link
//     anyway".  Safe links open normally.
//
// Refresh behaviour:
//   - Background worker spawned in main()'s setup block.
//   - On launch: refresh immediately if the local snapshot is
//     empty or older than 24 h; otherwise wait for the next
//     hourly tick.
//   - Errors are logged at warn level; the worker keeps the
//     previous snapshot so a transient outage at abuse.ch
//     doesn't wipe the list.

// URLhaus exposes three feed sizes:
//   - `csv_online`  — currently-active malicious URLs only (~10-20k)
//   - `csv_recent`  — last 30 days, online + offline (~30k)
//   - `csv`         — full historical dump (~5M, ~500MB)
//
// `csv_online` is too narrow for email use: malware infrastructure
// usually goes offline within hours of being identified, so a URL
// the user copied from the URLhaus website is more often than not
// already missing from `csv_online` even though URLhaus publicly
// shows it as a known-bad URL.  `csv_recent` covers the practical
// "this URL has been flagged in the last month" case at a quarter
// the storage size of going to the full dump.  Anything older than
// 30 days that's not also currently online drops off the local
// snapshot — acceptable trade-off for a 30k-row hourly refresh
// vs. a 5M-row monthly download.
const URLHAUS_CSV_URL: &str = "https://urlhaus.abuse.ch/downloads/csv_recent/";

/// Verdict surfaced to the frontend per URL.  `safe` means the
/// URL didn't match anything in URLhaus; `unsafe` means there
/// was either an exact-URL match or a host match (the v1 UI
/// collapses both into the red pill).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkVerdict {
    /// The URL the verdict was computed for, echoed back so the
    /// frontend can correlate without keeping its own index.
    url: String,
    /// `"safe"` | `"unsafe"`.  String tag rather than a bool so
    /// future tiers ("caution", "unknown") slot in without an
    /// IPC schema break.
    verdict: String,
    /// Optional context for the unsafe path: URLhaus' threat
    /// classification (e.g. `"malware_download"`) and tag list.
    /// `None` for safe URLs.
    threat: Option<String>,
    tags: Option<String>,
    /// `true` when the URL itself was on the list (vs only the
    /// host).  Used by the modal to render a slightly different
    /// hint ("This URL is on URLhaus" vs "This domain has
    /// hosted malicious content before").
    exact: bool,
}

#[tauri::command]
fn debug_link_check(
    url: String,
    cache: State<'_, Cache>,
) -> Result<serde_json::Value, NimbusError> {
    let status = link_check::status(&cache).map_err(NimbusError::from)?;
    let lookup = link_check::lookup(&cache, &url).map_err(NimbusError::from)?;
    let host_count = link_check::host_count_for_url(&cache, &url).map_err(NimbusError::from)?;
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&url)
        .split(['/', '?', '#', ':'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    Ok(serde_json::json!({
        "url": url,
        "extractedHost": host,
        "snapshotTotal": status.total_urls,
        "lastRefreshedAt": status.last_refreshed_at,
        "hostUrlCount": host_count,
        "lookupResult": lookup,
    }))
}

#[tauri::command]
fn check_urls(
    urls: Vec<String>,
    cache: State<'_, Cache>,
    settings: State<'_, SharedSettings>,
) -> Result<Vec<LinkVerdict>, NimbusError> {
    // Master toggle short-circuit: when the user has the link
    // checker turned off, return "unknown" verdicts that the
    // frontend renders without a pill at all.  We use the
    // existing `verdict` string ("off") rather than carrying a
    // separate enabled flag so the UI's per-URL render code
    // stays a single match.
    let enabled = futures::executor::block_on(settings.read()).link_check_enabled;
    if !enabled {
        return Ok(urls
            .into_iter()
            .map(|url| LinkVerdict {
                url,
                verdict: "off".into(),
                threat: None,
                tags: None,
                exact: false,
            })
            .collect());
    }

    let mut out = Vec::with_capacity(urls.len());
    for url in urls {
        match link_check::lookup(&cache, &url) {
            Ok(Some(m)) => out.push(LinkVerdict {
                url,
                verdict: "unsafe".into(),
                threat: Some(m.threat),
                tags: Some(m.tags),
                exact: m.exact,
            }),
            Ok(None) => out.push(LinkVerdict {
                url,
                verdict: "safe".into(),
                threat: None,
                tags: None,
                exact: false,
            }),
            Err(e) => {
                // Surface as "unknown" rather than failing the
                // whole batch; an SQLite error mid-walk is rare
                // and a single bad lookup shouldn't wipe pills
                // off every other link in the email.
                tracing::warn!("link_check lookup failed for {url}: {e}");
                out.push(LinkVerdict {
                    url,
                    verdict: "off".into(),
                    threat: None,
                    tags: None,
                    exact: false,
                });
            }
        }
    }
    Ok(out)
}

#[tauri::command]
fn get_link_check_status(
    cache: State<'_, Cache>,
) -> Result<link_check::UrlhausStatus, NimbusError> {
    link_check::status(&cache).map_err(NimbusError::from)
}

/// Manually trigger a URLhaus refresh.  Used by the "Refresh
/// now" button on the Settings page; also called by the
/// background worker on its hourly tick.
#[tauri::command]
async fn refresh_urlhaus_now(cache: State<'_, Cache>) -> Result<u32, NimbusError> {
    refresh_urlhaus_inner(&cache).await
}

async fn refresh_urlhaus_inner(cache: &Cache) -> Result<u32, NimbusError> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("nimbus-mail/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| NimbusError::Network(format!("urlhaus client build: {e}")))?;
    let resp = http
        .get(URLHAUS_CSV_URL)
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("urlhaus fetch: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(NimbusError::Network(format!(
            "urlhaus fetch returned HTTP {status}"
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| NimbusError::Network(format!("urlhaus body read: {e}")))?;
    let rows = parse_urlhaus_csv(&body);
    let cache_clone = cache.clone();
    let count = tokio::task::spawn_blocking(move || link_check::replace_all(&cache_clone, &rows))
        .await
        .map_err(|e| NimbusError::Other(format!("urlhaus replace_all join: {e}")))?
        .map_err(NimbusError::from)?;
    tracing::info!("URLhaus refresh complete — {count} URL(s)");
    Ok(count)
}

/// Hand-rolled minimal CSV parser for the URLhaus
/// `csv_online` dump.  The format is well-defined and stable:
///
/// ```text
/// # comment line
/// "id","dateadded","url","url_status","last_online","threat","tags","urlhaus_link","reporter"
/// ```
///
/// All fields are quoted; embedded commas and quotes are not
/// part of any URL we'll see in practice (URLhaus only catalogs
/// HTTP / HTTPS URLs, which can't legally contain unescaped
/// double quotes anyway).  Going hand-rolled here saves us a
/// `csv` crate workspace dependency for one feature.
fn parse_urlhaus_csv(body: &str) -> Vec<link_check::UrlhausCsvRow> {
    let mut out = Vec::with_capacity(8192);
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = split_csv_line(line);
        // Expect the canonical 9 fields; rows with the wrong
        // arity are upstream malformations we silently skip.
        if fields.len() < 7 {
            continue;
        }
        let date_added = parse_urlhaus_date(&fields[1]).unwrap_or(0);
        out.push(link_check::UrlhausCsvRow {
            url: fields[2].clone(),
            threat: fields[5].clone(),
            tags: fields[6].clone(),
            date_added,
        });
    }
    out
}

/// Split one CSV line into its quoted fields.  We tolerate
/// unquoted fields too (the URLhaus header / the occasional
/// malformed row), so a record with mixed quoting still
/// recovers cleanly.  Doubled `""` inside a quoted field
/// decodes to a single literal quote per RFC 4180.
fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, in_quotes) {
            ('"', true) => {
                if matches!(chars.peek(), Some('"')) {
                    chars.next();
                    current.push('"');
                } else {
                    in_quotes = false;
                }
            }
            ('"', false) => in_quotes = true,
            (',', false) => {
                fields.push(std::mem::take(&mut current));
            }
            (other, _) => current.push(other),
        }
    }
    fields.push(current);
    fields
}

/// Parse URLhaus' `dateadded` field (`YYYY-MM-DD HH:MM:SS` UTC)
/// into unix epoch seconds.  Falls back to `None` on a malformed
/// row so the caller can substitute zero rather than skipping.
fn parse_urlhaus_date(s: &str) -> Option<i64> {
    use chrono::NaiveDateTime;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

/// Background refresh worker.  Driven by an hourly tick plus a
/// startup-time decision: if the local snapshot is empty or
/// older than 24 h, refresh immediately; otherwise wait.  The
/// worker respects the `link_check_enabled` master toggle —
/// when off, it sleeps for the full tick window and re-checks
/// before doing any network work.
async fn urlhaus_refresh_worker(cache: Cache, settings: SharedSettings) {
    use tokio::time::{Duration, MissedTickBehavior, interval};

    // Initial decision based on the on-disk snapshot.  We
    // intentionally do *not* gate this on `link_check_enabled`:
    // a user who turned the feature off probably wants the
    // pre-existing list scrubbed too, but we also don't want
    // to re-download on every restart for a feature they
    // disabled.  Compromise: only the "stale" path triggers an
    // initial refresh, and we still respect the toggle inside
    // the refresh function below.
    let stale = match link_check::status(&cache) {
        Ok(s) => match s.last_refreshed_at {
            None => true, // never refreshed
            Some(ts) => {
                let age = chrono::Utc::now().signed_duration_since(ts).num_hours();
                age >= 24 || s.total_urls == 0
            }
        },
        Err(_) => true,
    };
    if stale {
        let enabled = settings.read().await.link_check_enabled;
        if enabled {
            if let Err(e) = refresh_urlhaus_inner(&cache).await {
                tracing::warn!("URLhaus initial refresh failed: {e}");
            }
        }
    }

    let mut tick = interval(Duration::from_secs(60 * 60));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Burn the immediate first tick so we don't stack on top
    // of the startup refresh above.
    tick.tick().await;

    loop {
        tick.tick().await;
        let enabled = settings.read().await.link_check_enabled;
        if !enabled {
            continue;
        }
        if let Err(e) = refresh_urlhaus_inner(&cache).await {
            tracing::warn!("URLhaus refresh failed (will retry next tick): {e}");
        }
    }
}

/// Switch the running app's icon (tray, window titlebar, taskbar)
/// to the user's picked logo style and persist the choice in
/// `AppSettings.logo_style`.  The next boot reapplies it.
///
/// Note this only swaps icons that exist *while the app runs*; the
/// `.exe` thumbnail Windows Explorer / macOS Finder shows for the
/// installed binary is baked in at `cargo tauri build` time and
/// can't change at runtime.
#[tauri::command]
async fn set_logo_style(
    app: AppHandle,
    style: String,
    settings: State<'_, SharedSettings>,
) -> Result<(), NimbusError> {
    let bytes = logo_bytes_for(&style);

    // Decode once up front so a bad slug fails before we touch any
    // running state.  `decode_logo_png` falls back to storm
    // internally if the slug is unknown, so this should always
    // succeed for reasonable inputs.
    let bitmap = decode_logo_png(bytes)?;

    // Swap the tray base bitmap so the next badge re-render uses
    // the new style.  Then trigger an immediate re-render so the
    // tray reflects the change without waiting for the next
    // unread-count tick.
    if let Some(tray_state) = app.try_state::<TrayBaseIcon>()
        && let Ok(mut guard) = tray_state.0.lock()
    {
        *guard = bitmap;
    }
    refresh_unread_badge(&app);

    // Update the main window's icon — Windows mirrors this into
    // the taskbar entry, macOS into the title bar, X11 into the
    // `_NET_WM_ICON` atom.
    if let Some(win) = app.get_webview_window("main")
        && let Ok(img) = tauri::image::Image::from_bytes(bytes)
        && let Err(e) = win.set_icon(img)
    {
        tracing::warn!("set_logo_style: window set_icon failed: {e}");
    }

    // Persist last so a transient apply failure can't permanently
    // wedge the user on a style they didn't pick.
    let mut s = settings.write().await;
    s.logo_style = style;
    app_settings::save_settings(&s)?;
    Ok(())
}

/// Custom URI scheme handler for `nimbus-logo://localhost/<style>`.
/// Used by the Settings picker to render preview tiles via plain
/// `<img src="nimbus-logo://localhost/storm">` — same trick the
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

// ── Custom themes (#132 tier 2) ────────────────────────────────
//
// User picks a Skeleton-shape CSS file in the Settings → Design
// "Import theme…" flow.  The frontend hands us the file's
// absolute path; we copy the bytes under
// `<config>/nimbus-mail/themes/<id>.css`, parse out the
// `[data-theme="…"]` slug to use as the picker id, and append a
// `CustomTheme` record to AppSettings.
//
// Removal deletes both the on-disk copy and the AppSettings row;
// the frontend's theme picker rebuilds from `get_app_settings`
// after each operation, so no extra plumbing.

/// Resolve the user-themes directory under the app's config root.
/// Created on demand — first import is what creates the folder.
fn custom_themes_dir() -> Result<std::path::PathBuf, NimbusError> {
    let base = dirs::config_dir()
        .ok_or_else(|| NimbusError::Other("cannot resolve user config dir".into()))?;
    let dir = base.join("nimbus-mail").join("themes");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(NimbusError::Other(format!(
            "create themes dir {}: {e}",
            dir.display()
        )));
    }
    Ok(dir)
}

/// Pull the theme slug out of an imported CSS file by scanning
/// for the first `[data-theme="…"]` selector.  Falls back to the
/// file stem when the file doesn't follow Skeleton's convention,
/// so the user still gets *something* in the picker — just won't
/// switch unless they edit the CSS to match the slug.
fn extract_theme_slug(css: &str, fallback: &str) -> String {
    let needle = "[data-theme=";
    if let Some(idx) = css.find(needle) {
        let tail = &css[idx + needle.len()..];
        // Accept both `"foo"` and `'foo'` quoting, tolerate
        // intra-attribute whitespace.
        let trimmed = tail.trim_start();
        if let Some(rest) = trimmed
            .strip_prefix('"')
            .or_else(|| trimmed.strip_prefix('\''))
            && let Some(end) = rest.find(['"', '\''])
        {
            let slug = rest[..end].trim();
            if !slug.is_empty() {
                return slug.to_string();
            }
        }
    }
    fallback.to_string()
}

/// Copy a user-picked CSS file into the app's themes directory
/// and append a `CustomTheme` record to AppSettings.  Returns the
/// freshly-created record so the frontend can register the
/// runtime stylesheet without re-reading settings.
///
/// Soft-fails on a duplicate slug by overwriting the previous
/// import — that's the natural "I edited the same file and want
/// to re-import" flow, and avoids forcing the user to remove the
/// old row first.
#[tauri::command]
async fn import_custom_theme(
    app: AppHandle,
    source_path: String,
    label: Option<String>,
    settings: State<'_, SharedSettings>,
) -> Result<CustomTheme, NimbusError> {
    let src = std::path::PathBuf::from(&source_path);
    if !src.exists() {
        return Err(NimbusError::Other(format!(
            "theme source not found: {source_path}"
        )));
    }
    let css = std::fs::read_to_string(&src)
        .map_err(|e| NimbusError::Other(format!("read theme source: {e}")))?;
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("custom-theme")
        .to_string();
    let slug = extract_theme_slug(&css, &stem);
    let dir = custom_themes_dir()?;
    let dest = dir.join(format!("{slug}.css"));
    std::fs::write(&dest, &css).map_err(|e| NimbusError::Other(format!("copy theme file: {e}")))?;

    let record = CustomTheme {
        id: slug.clone(),
        label: label.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
            // Title-case the slug so "my_theme" reads "My theme"
            // rather than something the user has to fix manually.
            stem.replace(['_', '-'], " ")
        }),
        description: "Imported theme".to_string(),
        path: dest.to_string_lossy().to_string(),
    };

    {
        let mut s = settings.write().await;
        // Replace any existing row with the same id (re-import).
        s.custom_themes.retain(|t| t.id != record.id);
        s.custom_themes.push(record.clone());
        app_settings::save_settings(&s)?;
    }

    // Tell every window so a second-window picker stays in sync.
    if let Err(e) = app.emit("custom-themes-changed", ()) {
        tracing::warn!("emit custom-themes-changed failed: {e}");
    }
    Ok(record)
}

/// Remove a user-imported theme — drops both the on-disk CSS and
/// the AppSettings row.  No-op when the id isn't found so the UI
/// can fire-and-forget without checking first.
#[tauri::command]
async fn remove_custom_theme(
    app: AppHandle,
    id: String,
    settings: State<'_, SharedSettings>,
) -> Result<(), NimbusError> {
    let path: Option<String> = {
        let mut s = settings.write().await;
        let path = s
            .custom_themes
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.path.clone());
        s.custom_themes.retain(|t| t.id != id);
        // If the removed theme was the active one, drop back to
        // the default so the UI doesn't try to render a missing
        // file on next launch.
        if s.theme_name == id {
            s.theme_name = "cerberus".into();
        }
        app_settings::save_settings(&s)?;
        path
    };
    if let Some(p) = path
        && let Err(e) = std::fs::remove_file(&p)
    {
        tracing::warn!("remove theme file {p}: {e}");
    }
    if let Err(e) = app.emit("custom-themes-changed", ()) {
        tracing::warn!("emit custom-themes-changed failed: {e}");
    }
    Ok(())
}

#[tauri::command]
fn get_total_unread(cache: State<'_, Cache>) -> Result<u32, NimbusError> {
    cache.total_unread_count().map_err(Into::into)
}

#[tauri::command]
fn show_main_window_cmd(app: AppHandle) -> Result<(), NimbusError> {
    show_main_window(&app)
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Restart Nimbus in place (#190 follow-up).
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
// `bundle.fileAssociations` in `tauri.conf.json` registers Nimbus
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

/// Capture argv[1] at startup if it points at an `.ics` or `.eml`
/// file we know how to open.  Anything else is ignored — Tauri
/// passes any `--flag` style argv too and we don't want to
/// accidentally treat those as paths.
fn capture_launch_file_arg() {
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

/// Frontend hook: returns the launch-time file path (if any) and
/// clears the slot so a window refresh doesn't re-open it.
#[tauri::command]
fn take_pending_file_to_open() -> Option<String> {
    pending_file_slot()
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

/// Read an `.eml` file from disk and parse it into the same
/// `Email` shape `get_mail` returns from the cache.  Used by the
/// view-only popout when the OS hands us a `.eml` to open.  No
/// account context — the popout disables reply / forward / archive
/// because there's no IMAP session to act against.
#[tauri::command]
fn parse_eml_file(path: String) -> Result<nimbus_core::models::Email, NimbusError> {
    let bytes =
        std::fs::read(&path).map_err(|e| NimbusError::Other(format!("read {path}: {e}")))?;
    let stem = std::path::Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    nimbus_imap::parse_eml_bytes(&bytes, &format!("file:{stem}"), "", "")
}

/// Read an `.ics` file from disk and parse it into one or more
/// `CalendarEvent`s.  Caller (the import-from-disk flow) opens the
/// first event in the EventEditor so the user can pick a target
/// calendar and save it via the existing create path.
#[tauri::command]
fn parse_ics_file(path: String) -> Result<Vec<nimbus_core::models::CalendarEvent>, NimbusError> {
    let body = std::fs::read_to_string(&path)
        .map_err(|e| NimbusError::Other(format!("read {path}: {e}")))?;
    nimbus_caldav::ical::parse_ics(&body)
}

/// Cross-platform "open the OS Default Apps panel" — used by the
/// settings page button so users can mark Nimbus as the default
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
fn open_default_apps_settings() -> Result<(), NimbusError> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", "ms-settings:defaultapps"])
            .spawn()
            .map_err(|e| NimbusError::Other(format!("open defaults panel: {e}")))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
            .spawn()
            .map_err(|e| NimbusError::Other(format!("open defaults panel: {e}")))?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Err(NimbusError::Other(
            "Default-app registration on Linux is per-desktop; \
             use `xdg-mime default nimbus-mail.desktop text/calendar message/rfc822` \
             from a terminal to mark Nimbus as the default handler."
                .into(),
        ))
    }
}

// ── App entry point ─────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt::init();

    // Pick up the path the OS handed us if Nimbus was invoked as
    // an `.ics` / `.eml` file handler (#254).  Capturing here —
    // before the Tauri builder runs — means the slot is populated
    // by the time the frontend's `take_pending_file_to_open`
    // ping arrives on first paint.
    capture_launch_file_arg();

    // Open (and migrate) the local mail cache once at startup, then
    // hand it to Tauri as managed state so every command can borrow it.
    // A failure here is fatal: without the cache the write-through path
    // is broken, and the user would silently lose offline capability.
    let cache = Cache::open_default().expect("failed to open local mail cache");
    // Stash a clone for the small set of helpers (e.g. `load_nextcloud_account`)
    // that fan out across many call sites and would otherwise need `&Cache`
    // threaded through 30+ functions.  `Cache` is a cheap `Arc`-clone, so this
    // doesn't duplicate the pool — just gives non-IPC code paths a way to
    // reach it without a State extractor.
    let _ = GLOBAL_CACHE.set(cache.clone());

    // Scrub orphan cache rows left behind by removed accounts.
    // `cache.wipe_account(...)` runs on account removal, but if it ever
    // missed (crash, disk error, older build before the wipe landed)
    // the unified inbox would surface envelopes whose owning account
    // no longer exists — every click on one throws "no account with
    // id 'X'". Running the scrub on boot guarantees the shell never
    // paints an orphan past the first frame, regardless of how the
    // cache got into that state.
    match account_store::load_accounts(&cache) {
        Ok(accounts) => {
            let active_ids: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
            if let Err(e) = cache.prune_orphan_accounts(&active_ids) {
                tracing::warn!("startup orphan-account prune failed: {e}");
            }
        }
        Err(e) => {
            tracing::warn!("skipping startup orphan-account prune — load_accounts failed: {e}")
        }
    }

    // One-time backfill for `addresses_json`.  The column was
    // added via ALTER TABLE with default `'[]'`; CardDAV's
    // delta-sync only re-pulls contacts that have changed in NC
    // since the last sync token, so unchanged ones kept the empty
    // default forever — even though their cached `vcard_raw` still
    // had the original ADR property.  Re-parse the body once to
    // recover the addresses.  Self-narrowing: a fixed row's
    // SELECT condition no longer matches on subsequent boots.
    match cache.backfill_addresses(|raw| {
        let p = nimbus_carddav::parse_vcard(raw).ok()?;
        Some(
            p.addresses
                .into_iter()
                .map(|a| nimbus_core::models::ContactAddress {
                    kind: a.kind,
                    street: a.street,
                    locality: a.locality,
                    region: a.region,
                    postal_code: a.postal_code,
                    country: a.country,
                })
                .collect(),
        )
    }) {
        Ok(0) => {}
        Ok(n) => tracing::info!("contact backfill: rewrote addresses_json on {n} rows"),
        Err(e) => tracing::warn!("contact backfill failed: {e}"),
    }

    // App-wide preferences (Issue #16). A missing file is fine on first
    // run — `load_settings` returns defaults. We wrap in Arc<RwLock<..>>
    // so the background sync loop can re-snapshot per tick while the
    // `update_app_settings` command swaps in a fresh value under the
    // write lock.
    let settings = app_settings::load_settings().unwrap_or_default();
    let shared_settings: SharedSettings = Arc::new(RwLock::new(settings));

    tauri::Builder::default()
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
        .manage(cache)
        .manage(shared_settings)
        .manage::<SystemFontsCache>(Arc::new(RwLock::new(Vec::new())))
        // Settings backup & sync (#168).  Frontend pushes its
        // localStorage snapshot on every settings change; the
        // worker reads from this slot when it assembles a bundle
        // for an NC push.  Starts empty — the first
        // `notify_settings_changed` IPC fills it in.
        .manage::<SharedLocalStorage>(Arc::new(RwLock::new(std::collections::HashMap::new())))
        .manage(SettingsSyncNotify(Arc::new(tokio::sync::Notify::new())))
        .register_uri_scheme_protocol("contact-photo", contact_photo_protocol)
        .register_uri_scheme_protocol("nimbus-logo", logo_protocol)
        .setup(|app| {
            // Windows toast attribution.  Without an explicit
            // AppUserModelID the OS falls back to the launching
            // process's AUMID — for `cargo tauri dev` that's the
            // shell (PowerShell, cmd, Git Bash), which is what
            // appears as the toast's source.  Setting our own AUMID
            // here makes notifications attribute to "Nimbus Mail"
            // in both dev and bundled builds.  The display-name +
            // icon come from a Start-Menu shortcut the installer
            // registers with this same AUMID; in dev the toast
            // shows the AUMID itself, which is still better than
            // "PowerShell".
            #[cfg(windows)]
            set_app_user_model_id();

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
            // Talk-join reminder state — empty fired/dismissed
            // sets at startup, populated as the background scan
            // discovers upcoming events with VALARM triggers.
            app.manage(EventReminderState::default());

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
                    &MenuItem::with_id(&handle, "open", "Open Nimbus", true, None::<&str>)?,
                    &MenuItem::with_id(&handle, "check", "Check Mail Now", true, None::<&str>)?,
                    &MenuItem::with_id(&handle, "compose", "Compose", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(&handle)?,
                    &MenuItem::with_id(&handle, "quit", "Quit Nimbus", true, None::<&str>)?,
                ],
            )?;

            // Honour the user's saved logo style at boot.  Falls
            // back to "storm" if decoding fails for any reason —
            // keeps the tray from coming up blank on a malformed
            // settings file.  Reads through the managed-state copy
            // so we don't have to capture `shared_settings` into
            // this closure (it was already moved into `.manage`).
            let chosen_style = {
                let st = app.state::<SharedSettings>();
                let s = futures::executor::block_on(st.read());
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
                .map_err(|e| NimbusError::Other(format!("decode tray icon: {e}")))?;

            // Stash the base RGBA in managed state so the badge
            // renderer (and `set_logo_style`) can re-composite
            // without re-reading the PNG on every unread-count
            // change or style swap.
            app.manage(TrayBaseIcon(std::sync::Mutex::new(initial_bitmap)));

            let _tray = TrayIconBuilder::with_id("nimbus-main")
                .icon(tray_icon)
                .tooltip("Nimbus Mail")
                .menu(&menu)
                // Windows: without this, left-click auto-pops the menu
                // and our click-handler never fires. We want left-click
                // to show the window, right-click to show the menu.
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => {
                        if let Err(e) = show_main_window(app) {
                            tracing::warn!("tray open failed: {e}");
                        }
                    }
                    "check" => {
                        let h = app.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = check_mail_now_inner(&h).await {
                                tracing::warn!("tray check_mail_now failed: {e}");
                            }
                        });
                    }
                    "compose" => {
                        if let Err(e) = show_main_window(app) {
                            tracing::warn!("tray compose open failed: {e}");
                        }
                        if let Err(e) = app.emit("open-compose", ()) {
                            tracing::warn!("failed to emit open-compose: {e}");
                        }
                    }
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
                        && let Err(e) = show_main_window(tray.app_handle())
                    {
                        tracing::warn!("tray left-click show failed: {e}");
                    }
                })
                .build(app)?;

            // ── Close-to-tray wiring ────────────────────────────
            //
            // We clone the settings Arc out of managed state so the
            // window-event closure (which is `Fn`, not `FnMut`, and
            // not async) can consult the current preference on every
            // close attempt. `blocking_read` is safe here: the window
            // event thread is already off the async runtime.
            if let Some(main_window) = app.get_webview_window("main") {
                let settings_for_close: SharedSettings =
                    app.state::<SharedSettings>().inner().clone();
                let close_window = main_window.clone();
                main_window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        let should_hide = settings_for_close.blocking_read().minimize_to_tray;
                        if should_hide {
                            api.prevent_close();
                            let _ = close_window.hide();
                        }
                    }
                });

                // The main window starts hidden (`visible: false` in
                // tauri.conf.json) so we don't paint it with the
                // bundled storm icon for a frame before the user's
                // chosen logo style is applied above.  Now that the
                // icon is in place, decide whether to show it:
                //   - `start_minimized` true → leave it hidden, app
                //     boots straight into the tray.
                //   - otherwise → show the window with the correct
                //     icon already painted in the titlebar / taskbar.
                let should_hide_on_start = app
                    .state::<SharedSettings>()
                    .inner()
                    .blocking_read()
                    .start_minimized;
                if !should_hide_on_start {
                    let _ = main_window.show();
                }
            } else {
                tracing::warn!("main window not found at setup time");
            }

            // Paint the initial badge from whatever's already in the
            // cache so the tray + taskbar reflect unread count from
            // the moment the app finishes booting (not only after the
            // first sync tick).
            refresh_unread_badge(app.handle());

            // ── Background sync ─────────────────────────────────
            //
            // `tauri::async_runtime::spawn` uses Tauri's managed
            // runtime, which is guaranteed to exist regardless of
            // how the app was started.
            let bg_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                background_sync_loop(bg_handle).await;
            });

            // ── Launch-time prerender (#178) ─────────────────────
            //
            // Warm the message cache for the newest INBOX envelopes
            // whose body we haven't fetched yet.  When the user
            // opens one of those mails the reading pane paints from
            // cache instantly instead of waiting on an IMAP
            // round-trip — the difference between a perceptibly
            // snappy first-mail click and the previous "open …
            // briefly blank … now it appears" UX.
            //
            // Spawned as a low-priority background task so it never
            // gates the UI: each account is processed sequentially
            // (one IMAP connection at a time per account, since the
            // IMAP client is single-shot here), but accounts run in
            // parallel.  Failures are logged and skipped — a
            // half-warmed cache is strictly better than no warm-up.
            let prerender_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                prerender_inboxes_on_launch(&prerender_handle).await;
            });

            // Settings auto-sync worker (#168).  Listens for
            // notifications from the frontend via the
            // `SettingsSyncNotify` state, debounces 2s, then
            // pushes the bundle to whichever NC the user picked
            // as their backup target.  Also retries a pending
            // push every 5 minutes so a "user went offline,
            // never changed another setting, came back online"
            // flow eventually catches up without manual action.
            //
            // Pumps an immediate notification on startup so a
            // pending=true flag from a previous session (set
            // when a quit-while-offline left a push hanging)
            // gets a fresh attempt as soon as we're up.
            let sync_cache = app.state::<Cache>().inner().clone();
            let sync_storage = app.state::<SharedLocalStorage>().inner().clone();
            let sync_notify = app.state::<SettingsSyncNotify>().inner().0.clone();
            let initial_kick = sync_notify.clone();
            tauri::async_runtime::spawn(async move {
                settings_sync_worker(sync_cache, sync_storage, sync_notify).await;
            });
            // Kick the worker once so a pending recovery push
            // from a previous session retries on launch.  The
            // worker no-ops cleanly if there's nothing to do.
            if settings_sync::load_state()
                .map(|s| s.pending && s.target_nc_id.is_some())
                .unwrap_or(false)
            {
                initial_kick.notify_one();
            }

            // URLhaus link-safety refresh worker (#165).  Pulls
            // the abuse.ch CSV every hour, decides on launch
            // whether the local copy is stale enough to refresh
            // immediately, and respects the link_check_enabled
            // master toggle in AppSettings.
            let urlhaus_cache = app.state::<Cache>().inner().clone();
            let urlhaus_settings = app.state::<SharedSettings>().inner().clone();
            tauri::async_runtime::spawn(async move {
                urlhaus_refresh_worker(urlhaus_cache, urlhaus_settings).await;
            });

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
            probe_server_certificate,
            test_connection,
            fetch_envelopes,
            fetch_unified_envelopes,
            fetch_older_envelopes,
            fetch_older_unified_envelopes,
            fetch_message,
            download_email_attachment,
            put_attachment_preview,
            get_attachment_previews,
            download_calendar_from_message,
            fetch_folders,
            create_folder,
            delete_folder,
            rename_folder,
            mark_as_read,
            set_message_read,
            send_email,
            list_outbox,
            list_all_outbox,
            count_outbox,
            retry_outbox_entry,
            delete_outbox_entry,
            edit_outbox_entry,
            save_draft,
            delete_message,
            archive_message,
            move_message,
            move_messages,
            get_cached_envelopes,
            get_unified_cached_envelopes,
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
            upload_to_nextcloud,
            office_open_attachment,
            office_close_attachment,
            office_sweep_temp,
            pdf_open_attachment,
            pdf_close_attachment,
            print_attachment,
            save_bytes_to_path,
            read_text_from_path,
            sync_nextcloud_contacts,
            get_contacts_sync_status,
            get_calendars_sync_status,
            get_contacts,
            search_contacts,
            get_contact_photo,
            create_contact,
            update_contact,
            delete_contact,
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
            // Issue #168: settings backup & sync.
            build_settings_bundle,
            apply_settings_bundle,
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
            show_main_window_cmd,
            quit_app,
            restart_app,
            // #254 — file-association entry points
            take_pending_file_to_open,
            parse_eml_file,
            parse_ics_file,
            open_default_apps_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Nimbus");
}

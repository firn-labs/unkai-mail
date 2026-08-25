//! Nextcloud account linking, Files, and public shares.
//!
//! Mirrors `ui/src/lib/api/nextcloud.ts`.

use serde::Serialize;
use unkai_caldav::resolve_calendar_home as caldav_resolve_calendar_home;
use unkai_carddav::resolve_addressbook_home as carddav_resolve_addressbook_home;
use unkai_core::UnkaiError;
use unkai_core::models::DavSourceKind;
use unkai_core::models::NextcloudAccount;
use unkai_core::models::NextcloudCapabilities;
use unkai_nextcloud::FileEntry;
use unkai_nextcloud::LoginFlowInit;
use unkai_nextcloud::LoginFlowResult;
use unkai_nextcloud::fetch_capabilities;
use unkai_nextcloud::poll_login;
use unkai_nextcloud::start_login;
use unkai_store::Cache;
use unkai_store::cache::CalendarRow;
use unkai_store::credentials;
use unkai_store::nextcloud_store;

use crate::state::global_cache;
use crate::support::{LOCAL_ADDRESSBOOK_NAME, load_nextcloud_account};

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
pub async fn start_nextcloud_login(
    server_url: String,
    trusted_certs: Option<Vec<unkai_core::models::TrustedCert>>,
) -> Result<LoginFlowInit, UnkaiError> {
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
pub async fn poll_nextcloud_login(
    poll_endpoint: String,
    poll_token: String,
    trusted_certs: Option<Vec<unkai_core::models::TrustedCert>>,
) -> Result<Option<NextcloudAccount>, UnkaiError> {
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
        kind: unkai_core::models::DavSourceKind::Nextcloud,
        carddav_home: None,
        caldav_home: None,
    };
    nextcloud_store::upsert_account(global_cache()?, account.clone())?;
    Ok(Some(account))
}

/// List all saved Nextcloud connections.
pub fn get_nextcloud_accounts() -> Result<Vec<NextcloudAccount>, UnkaiError> {
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
pub async fn refresh_nextcloud_capabilities(nc_id: String) -> Result<NextcloudAccount, UnkaiError> {
    let mut account = load_nextcloud_account(&nc_id)?;
    // DAV/local sources have no OCS capabilities endpoint (#413) —
    // their synthetic snapshot was fixed at add time.
    if !account.is_nextcloud() {
        return Ok(account);
    }
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
pub async fn get_nextcloud_user_email(nc_id: String) -> Result<Option<String>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    // No OCS profile on DAV/local sources (#413) — callers fall
    // back to the first mail account's address.
    if !account.is_nextcloud() {
        return Ok(None);
    }
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    match unkai_nextcloud::user::fetch_current_user(
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

/// Replace the per-account TLS trust list (#253).
///
/// Used by the AccountSettings panel's "Trusted certificates" section
/// and the Nextcloud setup wizard's cert-probe prompt: when a TLS
/// handshake fails because the server is using a self-signed cert,
/// the UI calls `probe_server_certificate` to capture the chain, asks
/// the user, and on confirm ships the new fingerprints back through
/// here.  Subsequent OCS / CalDAV / CardDAV / Notes / Talk / Files
/// requests pick the new list up automatically — every protocol-crate
/// API rebuilds its `reqwest::Client` per call from the account's
/// `trusted_certs`.
pub fn update_nextcloud_account_trusted_certs(
    nc_id: String,
    trusted_certs: Vec<unkai_core::models::TrustedCert>,
    cache: &Cache,
) -> Result<NextcloudAccount, UnkaiError> {
    let mut account = load_nextcloud_account(&nc_id)?;
    account.trusted_certs = trusted_certs;
    nextcloud_store::upsert_account(cache, account.clone())?;
    Ok(account)
}

/// Forget a saved Nextcloud connection. The keychain entry goes too so the user
/// can delete the app password from their NC security settings.
///
/// Also drops cached contacts, calendars, and their DAV sync state for
/// this account; a best-effort failure there is logged but doesn't
/// block removal.
pub fn remove_nextcloud_account(id: String, cache: &Cache) -> Result<(), UnkaiError> {
    // Best-effort: local-only sources (#413) never had a keychain
    // entry, and a missing entry shouldn't block removing an
    // otherwise-dead account record either way.
    if let Err(e) = credentials::delete_nextcloud_password(&id) {
        tracing::warn!("failed to delete keychain entry for '{id}' (continuing): {e}");
    }
    if let Err(e) = cache.wipe_nextcloud_contacts(&id) {
        tracing::warn!("failed to wipe contacts for NC account '{id}': {e}");
    }
    if let Err(e) = cache.wipe_nextcloud_calendars(&id) {
        tracing::warn!("failed to wipe calendars for NC account '{id}': {e}");
    }
    if let Err(e) = cache.wipe_notes_for_account(&id) {
        tracing::warn!("failed to wipe notes for NC account '{id}': {e}");
    }
    nextcloud_store::remove_account(cache, &id)
}

// ── Separate CardDAV/CalDAV sources + local store (#413) ────────
//
// Contacts and calendars no longer have to come from the mail
// account's Nextcloud. `add_dav_account` connects a generic
// CardDAV/CalDAV server (RFC 6764 home discovery, HTTP Basic auth,
// app password in the keychain); `add_local_dav_account` registers a
// source with no remote at all. Both reuse the `NextcloudAccount`
// record + cache keying (see `DavSourceKind`), so every existing
// sync command and view picks them up without special cases beyond
// the kind checks.

/// Synthetic capability snapshot for DAV/local sources: only the
/// contact/calendar bits the user actually enabled, everything
/// Nextcloud-specific off so no integration icon lights up dead.
pub fn dav_capabilities(use_contacts: bool, use_calendars: bool) -> NextcloudCapabilities {
    NextcloudCapabilities {
        version: None,
        talk: false,
        files: false,
        caldav: use_calendars,
        carddav: use_contacts,
        office: false,
        notes: false,
        tasks: false,
    }
}

/// Connect a generic CardDAV/CalDAV server (#413).
///
/// Resolves the collection homes up front (RFC 6764 well-known →
/// principal → home-set, with sensible fallbacks for pasted DAV
/// paths) so a bad URL or password fails *here*, in the setup UI,
/// instead of silently on the first background sync. The password
/// goes to the OS keychain under the same service Nextcloud app
/// passwords use, keyed by the new account id.
#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
pub async fn add_dav_account(
    display_name: String,
    server_url: String,
    username: String,
    password: String,
    use_contacts: bool,
    use_calendars: bool,
    trusted_certs: Option<Vec<unkai_core::models::TrustedCert>>,
    cache: &Cache,
) -> Result<NextcloudAccount, UnkaiError> {
    if !use_contacts && !use_calendars {
        return Err(UnkaiError::Other(
            "enable at least one of contacts or calendars".into(),
        ));
    }
    let server = server_url.trim().trim_end_matches('/').to_string();
    if server.is_empty() {
        return Err(UnkaiError::Other("server URL must not be empty".into()));
    }
    // Tolerate a bare hostname the same way the Nextcloud connect
    // card does.
    let server = if server.starts_with("http://") || server.starts_with("https://") {
        server
    } else {
        format!("https://{server}")
    };
    let trust = trusted_certs.unwrap_or_default();

    // The two resolution ladders are independent — run them
    // concurrently so the wizard spinner lasts one discovery chain,
    // not the sum of both.
    let (carddav_home, caldav_home) = tokio::join!(
        async {
            if use_contacts {
                carddav_resolve_addressbook_home(&server, &username, &password, &trust)
                    .await
                    .map(Some)
            } else {
                Ok(None)
            }
        },
        async {
            if use_calendars {
                caldav_resolve_calendar_home(&server, &username, &password, &trust)
                    .await
                    .map(Some)
            } else {
                Ok(None)
            }
        }
    );
    let carddav_home = carddav_home?;
    let caldav_home = caldav_home?;

    // `#dav` suffix keeps the id from colliding with a Nextcloud
    // connection to the same server/user (their id is `server#user`).
    let id = format!("{server}#{username}#dav");
    credentials::store_nextcloud_password(&id, &password)?;

    let account = NextcloudAccount {
        id,
        server_url: server,
        username,
        display_name: Some(display_name),
        capabilities: Some(dav_capabilities(use_contacts, use_calendars)),
        trusted_certs: trust,
        kind: DavSourceKind::Dav,
        carddav_home,
        caldav_home,
    };
    nextcloud_store::upsert_account(cache, account.clone())?;
    Ok(account)
}

/// Register a purely local contacts/calendar store (#413) — no
/// remote, no credentials. Seeds one addressbook and/or calendar so
/// the views have somewhere to write from the first click.
pub fn add_local_dav_account(
    display_name: String,
    use_contacts: bool,
    use_calendars: bool,
    cache: &Cache,
) -> Result<NextcloudAccount, UnkaiError> {
    if !use_contacts && !use_calendars {
        return Err(UnkaiError::Other(
            "enable at least one of contacts or calendars".into(),
        ));
    }
    let id = format!("local#{}", uuid::Uuid::new_v4());

    let account = NextcloudAccount {
        id: id.clone(),
        server_url: String::new(),
        username: String::new(),
        display_name: Some(display_name),
        capabilities: Some(dav_capabilities(use_contacts, use_calendars)),
        trusted_certs: Vec::new(),
        kind: DavSourceKind::Local,
        carddav_home: None,
        caldav_home: None,
    };
    nextcloud_store::upsert_account(cache, account.clone())?;

    if use_contacts {
        // An empty delta registers the addressbook's sync-state row,
        // which is what `list_nextcloud_addressbooks` and the
        // contacts view key their book lists on.
        if let Err(e) = cache.apply_contact_delta(
            &id,
            LOCAL_ADDRESSBOOK_NAME,
            Some("Contacts"),
            &[],
            &[],
            None,
            None,
        ) {
            tracing::warn!("failed to seed local addressbook for '{id}': {e}");
        }
    }
    if use_calendars {
        let row = CalendarRow {
            path: format!("local://{}/", uuid::Uuid::new_v4()),
            display_name: "Calendar".to_string(),
            color: None,
            ctag: None,
            hidden: false,
            muted: false,
            read_only: false,
        };
        if let Err(e) = cache.insert_calendar(&id, &row) {
            tracing::warn!("failed to seed local calendar for '{id}': {e}");
        }
    }
    Ok(account)
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
pub async fn list_nextcloud_files(
    nc_id: String,
    path: String,
) -> Result<Vec<FileEntry>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::list_directory(
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
pub async fn download_nextcloud_file(nc_id: String, path: String) -> Result<Vec<u8>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::download_file(
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
pub async fn nextcloud_file_preview(
    nc_id: String,
    path: String,
    size: Option<u32>,
) -> Result<Option<Vec<u8>>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let s = size.unwrap_or(96);
    match unkai_nextcloud::fetch_preview(
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
        Err(UnkaiError::Nextcloud(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Result of `create_nextcloud_share` — both the public URL (for
/// pasting into the email body) and the share id (for later
/// label updates via `update_nextcloud_share_label`).
#[derive(serde::Serialize)]
pub struct NextcloudShareResult {
    pub id: String,
    pub url: String,
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
/// - `expire_date`: optional `YYYY-MM-DD` after which Nextcloud
///   refuses to serve the link (#324).  Omitting / `None` leaves the
///   share open until manually revoked (subject to any server-side
///   default-expiration policy the admin has configured).
pub async fn create_nextcloud_share(
    nc_id: String,
    path: String,
    password: Option<String>,
    label: Option<String>,
    permissions: Option<u8>,
    expire_date: Option<String>,
) -> Result<NextcloudShareResult, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let share = unkai_nextcloud::create_public_share(
        &account.server_url,
        &account.username,
        &app_password,
        &path,
        password.as_deref(),
        label.as_deref(),
        permissions.unwrap_or(unkai_nextcloud::shares::PERM_READ_ONLY),
        expire_date.as_deref(),
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
pub async fn update_nextcloud_share_label(
    nc_id: String,
    share_id: String,
    label: String,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::update_share_label(
        &account.server_url,
        &account.username,
        &app_password,
        &share_id,
        &label,
        &account.trusted_certs,
    )
    .await
}

/// Snapshot of a single public share for the management UI (#117).
/// Mirrors `unkai_nextcloud::PublicShareInfo` but carries the
/// originating `nc_id` so the UI can dispatch follow-up updates /
/// deletes against the right account when the user has more than
/// one Nextcloud connected.
#[derive(serde::Serialize)]
pub struct NextcloudShareRow {
    pub nc_id: String,
    pub id: String,
    pub path: String,
    pub item_type: String,
    pub url: String,
    pub token: String,
    pub label: Option<String>,
    pub permissions: u8,
    pub has_password: bool,
    pub expiration: Option<String>,
    pub stime: i64,
    pub mimetype: String,
}

/// List every public share link the given Nextcloud account owns
/// (#117).  Powers the dedicated share-management view in the rail.
pub async fn list_nextcloud_shares(nc_id: String) -> Result<Vec<NextcloudShareRow>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let shares = unkai_nextcloud::list_public_shares(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    Ok(shares
        .into_iter()
        .map(|s| NextcloudShareRow {
            nc_id: nc_id.clone(),
            id: s.id,
            path: s.path,
            item_type: s.item_type,
            url: s.url,
            token: s.token,
            label: s.label,
            permissions: s.permissions,
            has_password: s.has_password,
            expiration: s.expiration,
            stime: s.stime,
            mimetype: s.mimetype,
        })
        .collect())
}

/// Update a Nextcloud public share's password / permissions /
/// expiry from the share-management view (#117).  Each field is
/// optional — only the ones the caller passes get sent to the
/// server.  Empty-string `password` clears the password gate;
/// empty-string `expire_date` clears the expiration.
pub async fn update_nextcloud_share(
    nc_id: String,
    share_id: String,
    password: Option<String>,
    permissions: Option<u8>,
    expire_date: Option<String>,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::update_public_share(
        &account.server_url,
        &account.username,
        &app_password,
        &share_id,
        password.as_deref(),
        permissions,
        expire_date.as_deref(),
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
pub async fn delete_nextcloud_share(nc_id: String, share_id: String) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::delete_share(
        &account.server_url,
        &account.username,
        &app_password,
        &share_id,
        &account.trusted_certs,
    )
    .await
}

/// Upload raw bytes to a file in the user's Nextcloud.
///
/// The "Save to Nextcloud" action on a received email attachment calls
/// this with `path = <chosen folder>/<attachment filename>`. Existing
/// files at the same path are overwritten — the UI confirms with the
/// user before calling when that might be surprising.
pub async fn upload_to_nextcloud(
    nc_id: String,
    path: String,
    data: Vec<u8>,
    content_type: Option<String>,
) -> Result<String, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::upload_file(
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
pub async fn create_nextcloud_directory(nc_id: String, path: String) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::create_directory(
        &account.server_url,
        &account.username,
        &app_password,
        &path,
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
pub struct NextcloudUserLookup {
    pub user_id: String,
    pub display_name: String,
}

pub async fn find_nextcloud_user_by_email(
    nc_id: String,
    email: String,
) -> Result<Option<NextcloudUserLookup>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let m = unkai_nextcloud::find_user_by_email(
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

// ── Nextcloud user groups + Teams (#133 follow-up) ────────────
//
// These are *identity / access* groups, separate from the vCard
// `KIND:group` records above.  Members are NC user IDs
// (provisioning-API speak), not vCard UIDs, so the contacts UI
// renders them in their own read-only sections — Unkai can't
// add or remove members (admin task) but it can surface the
// groups the user already belongs to and resolve their members
// to email addresses for the Compose autocomplete.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NextcloudGroupView {
    /// Nextcloud account this group lives on.
    pub nextcloud_account_id: String,
    /// Group / circle identifier — used as the picker id; UNIQUE
    /// per (`nextcloud_account_id`, `source`).
    pub id: String,
    /// `"group"` for OCS user groups, `"team"` for Circles /
    /// Teams.  Rendered as a colored pill in the sidebar.
    pub source: String,
    pub display_name: String,
    pub members: Vec<NextcloudGroupMemberView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NextcloudGroupMemberView {
    pub user_id: String,
    pub display_name: String,
    /// Empty when the NC user has no email set in Personal info.
    pub email: String,
}

/// Strip the SAML / LDAP prefixes some NC instances bake into
/// group ids when they sync from an upstream IdP — the user
/// sees a clean display name instead of `SAML_Engineering`.
/// Idempotent and case-insensitive on the prefix; everything
/// else passes through untouched.
pub fn humanize_nc_group_name(raw: &str) -> String {
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
pub async fn list_nextcloud_groups(cache: &Cache) -> Result<Vec<NextcloudGroupView>, UnkaiError> {
    let accounts = nextcloud_store::load_accounts(cache).unwrap_or_default();
    let mut out: Vec<NextcloudGroupView> = Vec::new();
    // Build a uid → email fallback map from the local CardDAV
    // cache.  Most NC instances sync the system addressbook into
    // CardDAV with each user's vCard UID == their NC user_id, so
    // this lets us recover emails even when the OCS user-profile
    // endpoint hides them (regular users querying other users
    // only get a display name, not the email field).
    // No hydrate closure — this map only reads UIDs, names, and
    // emails, which are columnar.
    let cache_uid_email: std::collections::HashMap<String, (String, String)> = cache
        .list_contacts(None, |_, _| {})
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
        let group_ids = match unkai_nextcloud::fetch_my_groups(
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
        let circles = match unkai_nextcloud::fetch_my_circles(
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
            let mids = match unkai_nextcloud::fetch_circle_member_ids(
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
pub async fn collect_group_members(
    acc: &NextcloudAccount,
    app_password: &str,
    group_id: &str,
    cache_uid_email: &std::collections::HashMap<String, (String, String)>,
) -> Vec<NextcloudGroupMemberView> {
    let ids = match unkai_nextcloud::fetch_group_member_ids(
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
pub async fn resolve_member_profiles(
    acc: &NextcloudAccount,
    app_password: &str,
    ids: Vec<String>,
    cache_uid_email: &std::collections::HashMap<String, (String, String)>,
) -> Vec<NextcloudGroupMemberView> {
    let futs = ids.into_iter().map(|uid| async move {
        let prof = unkai_nextcloud::fetch_user_profile(
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

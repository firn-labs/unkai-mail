//! Helpers shared by more than one domain module (#476).
//!
//! The rule for landing here rather than in a domain module is simply
//! "more than one domain calls it" — account/connection loading, the
//! DAV write helpers that both contacts and calendar go through, and
//! the folder-role pickers compose and mail both need.

use serde::Serialize;
use unkai_caldav::create_event as caldav_create_event;
use unkai_caldav::update_event as caldav_update_event;
use unkai_carddav::create_contact as carddav_create_contact;
use unkai_carddav::delete_contact as carddav_delete_contact;
use unkai_carddav::update_contact as carddav_update_contact;
use unkai_core::UnkaiError;
use unkai_core::models::Account;
use unkai_core::models::NextcloudAccount;
use unkai_imap::ImapClient;
use unkai_jmap::JmapClient;
use unkai_store::Cache;
use unkai_store::account_store;
use unkai_store::credentials;
use unkai_store::nextcloud_store;

use crate::state::global_cache;

/// Collection name for the single addressbook a local source gets.
pub const LOCAL_ADDRESSBOOK_NAME: &str = "local";

/// Aggregate sync status for the Settings UI's Contacts and
/// Calendars rows. Both surfaces want the same shape: when did we
/// last successfully sync, and what's the cached count? — so we
/// share the struct and reuse the `SyncStatusRow` Svelte component.
#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    /// RFC 3339 timestamp of the most recent successful sync across
    /// every addressbook / calendar for this account, or `None` if
    /// the account has never finished one. The frontend formats it
    /// relative ("12m ago" / "Synced just now").
    pub last_synced_at: Option<String>,
    /// Cached row count for this account (contacts or calendars).
    /// Mostly informational — the row title carries the meaningful
    /// "are we up to date?" signal.
    pub count: u32,
}

pub fn load_nextcloud_account(nc_id: &str) -> Result<NextcloudAccount, UnkaiError> {
    nextcloud_store::load_accounts(global_cache()?)?
        .into_iter()
        .find(|a| a.id == nc_id)
        .ok_or_else(|| UnkaiError::Other(format!("no Nextcloud account with id '{nc_id}'")))
}

// ── DAV write wrappers with local-store branching (#413) ────────
//
// Every contact/event write used to be a bare CardDAV/CalDAV call.
// A `Local` source has no server, so these wrappers centralise the
// branch: local writes skip the round-trip and mint a synthetic
// href (unique per resource, `local://`-shaped so nothing mistakes
// it for a real URL) plus a fresh etag per revision — the shared
// cache upsert paths downstream don't change at all. Remote sources
// (Nextcloud and generic DAV alike) go through the original protocol
// calls; generic DAV works unmodified because collection paths are
// stored absolute and auth is plain HTTP Basic.

/// `scheme://host[:port]` of an absolute URL — the base DAV hrefs
/// get resolved against. Falls back to the input (trimmed) when it
/// doesn't parse, which matches the old pass-the-server-url
/// behaviour for Nextcloud origins.
pub fn url_origin(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let mut origin = format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default());
            if let Some(port) = u.port() {
                origin.push_str(&format!(":{port}"));
            }
            origin
        }
        Err(_) => url.trim_end_matches('/').to_string(),
    }
}

/// Resolved CardDAV addressbook-home URL for any remote source
/// (#413): generic DAV records store the RFC 6764-resolved home;
/// Nextcloud derives it from the fixed server layout. Never called
/// for local sources (they have no home).
pub fn carddav_home_of(account: &NextcloudAccount) -> String {
    match &account.carddav_home {
        Some(home) => home.clone(),
        None => format!(
            "{}/remote.php/dav/addressbooks/users/{}/",
            account.server_url.trim_end_matches('/'),
            account.username
        ),
    }
}

/// CalDAV twin of [`carddav_home_of`].
pub fn caldav_home_of(account: &NextcloudAccount) -> String {
    match &account.caldav_home {
        Some(home) => home.clone(),
        None => format!(
            "{}/remote.php/dav/calendars/{}/",
            account.server_url.trim_end_matches('/'),
            account.username
        ),
    }
}

pub fn local_vcf_outcome(collection_url: &str, uid: &str) -> unkai_carddav::WriteOutcome {
    unkai_carddav::WriteOutcome {
        href: format!("{}/{uid}.vcf", collection_url.trim_end_matches('/')),
        etag: uuid::Uuid::new_v4().to_string(),
    }
}

pub fn local_ics_outcome(collection_url: &str, uid: &str) -> unkai_caldav::WriteOutcome {
    unkai_caldav::WriteOutcome {
        href: format!("{}/{uid}.ics", collection_url.trim_end_matches('/')),
        etag: uuid::Uuid::new_v4().to_string(),
    }
}

pub async fn dav_create_contact_for(
    account: &NextcloudAccount,
    addressbook_url: &str,
    uid: &str,
    vcard: &str,
) -> Result<unkai_carddav::WriteOutcome, UnkaiError> {
    if account.is_local() {
        return Ok(local_vcf_outcome(addressbook_url, uid));
    }
    let app_password = credentials::get_nextcloud_password(&account.id)?;
    carddav_create_contact(
        &account.server_url,
        addressbook_url,
        &account.username,
        &app_password,
        uid,
        vcard,
        &account.trusted_certs,
    )
    .await
}

pub async fn dav_update_contact_for(
    account: &NextcloudAccount,
    href: &str,
    etag: &str,
    vcard: &str,
) -> Result<unkai_carddav::WriteOutcome, UnkaiError> {
    if account.is_local() {
        // Same href, fresh etag — the revision marker only matters
        // for optimistic concurrency, which a single local store
        // doesn't need, but keeping it fresh keeps the cache rows
        // shaped exactly like remote ones.
        return Ok(unkai_carddav::WriteOutcome {
            href: href.to_string(),
            etag: uuid::Uuid::new_v4().to_string(),
        });
    }
    let app_password = credentials::get_nextcloud_password(&account.id)?;
    carddav_update_contact(
        href,
        &account.username,
        &app_password,
        etag,
        vcard,
        &account.trusted_certs,
    )
    .await
}

pub async fn dav_delete_contact_for(
    account: &NextcloudAccount,
    href: &str,
    etag: &str,
) -> Result<(), UnkaiError> {
    if account.is_local() {
        return Ok(());
    }
    let app_password = credentials::get_nextcloud_password(&account.id)?;
    carddav_delete_contact(
        href,
        &account.username,
        &app_password,
        etag,
        &account.trusted_certs,
    )
    .await
}

pub async fn dav_create_event_for(
    account: &NextcloudAccount,
    calendar_path: &str,
    uid: &str,
    ics: &str,
) -> Result<unkai_caldav::WriteOutcome, UnkaiError> {
    if account.is_local() {
        return Ok(local_ics_outcome(calendar_path, uid));
    }
    let app_password = credentials::get_nextcloud_password(&account.id)?;
    caldav_create_event(
        &account.server_url,
        calendar_path,
        &account.username,
        &app_password,
        uid,
        ics,
        &account.trusted_certs,
    )
    .await
}

pub async fn dav_update_event_for(
    account: &NextcloudAccount,
    href: &str,
    etag: &str,
    ics: &str,
) -> Result<unkai_caldav::WriteOutcome, UnkaiError> {
    if account.is_local() {
        return Ok(unkai_caldav::WriteOutcome {
            href: href.to_string(),
            etag: uuid::Uuid::new_v4().to_string(),
        });
    }
    let app_password = credentials::get_nextcloud_password(&account.id)?;
    caldav_update_event(
        href,
        &account.username,
        &app_password,
        etag,
        ics,
        &account.trusted_certs,
    )
    .await
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
pub fn load_account(cache: &Cache, id: &str) -> Result<Account, UnkaiError> {
    account_store::load_accounts(cache)?
        .into_iter()
        .find(|a| a.id == id)
        .ok_or_else(|| UnkaiError::Other(format!("no account with id '{id}'")))
}

/// Connect to an account's IMAP server using the stored password.
/// Includes any per-account TLS-trusted certs so a self-signed
/// server the user has previously accepted continues to validate.
pub async fn connect_imap(account: &Account) -> Result<ImapClient, UnkaiError> {
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
pub async fn connect_jmap(account: &Account) -> Result<JmapClient, UnkaiError> {
    let jmap_url = account.jmap_url.as_deref().ok_or_else(|| {
        UnkaiError::Other(format!(
            "Account '{}' has use_jmap=true but no jmap_url configured",
            account.id
        ))
    })?;
    let password = credentials::get_imap_password(&account.id)?;
    JmapClient::connect(jmap_url, &account.email, &password).await
}

/// Returns `true` if this account should use JMAP instead of IMAP.
pub fn uses_jmap(account: &Account) -> bool {
    account.use_jmap && account.jmap_url.is_some()
}

/// Did this delete_message result leave the cache holding a definitely-
/// stale row for the target UID? True when the server confirmed the
/// delete (Ok) *or* reported the UID isn't there (the probe error we
/// added to `delete_message`) — in both cases the cached envelope
/// should come out.
pub fn should_clean_cache_for_delete(result: &Result<(), UnkaiError>) -> bool {
    match result {
        Ok(()) => true,
        Err(UnkaiError::Protocol(msg)) => msg.contains("isn't in folder"),
        _ => false,
    }
}

/// Pick the most likely Drafts folder name from the cached folder list.
/// Same strategy as `pick_sent_folder`: prefer the IMAP `\Drafts`
/// special-use attribute, fall back to common English / German / French
/// names so accounts that haven't been synced yet still land in the
/// right place.  The heuristic itself lives in
/// `unkai_core::mail_util` (#440) so the MCP `create_draft` tool
/// shares it.
pub fn pick_drafts_folder(account_id: &str, cache: &Cache) -> Option<String> {
    let folders = cache.get_folders(account_id).ok()?;
    unkai_core::mail_util::pick_drafts_folder(&folders)
}

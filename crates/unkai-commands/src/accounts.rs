//! Account CRUD, autodiscovery, and connection testing.
//!
//! Mirrors `ui/src/lib/api/accounts.ts`.

use serde::Deserialize;
use serde::Serialize;
use unkai_core::UnkaiError;
use unkai_core::models::Account;
use unkai_imap::ImapClient;
use unkai_jmap::JmapClient;
use unkai_store::Cache;
use unkai_store::account_store;
use unkai_store::credentials;

use crate::state::SettingsSyncNotify;

// ── Tauri commands ──────────────────────────────────────────────
//
// Each `#[tauri::command]` function becomes callable from the
// Svelte frontend via `invoke("command_name", { args })`.
//
// Tauri serialises the return value as JSON and sends it to the
// frontend. Errors must implement `Serialize` (which UnkaiError
// does) so Tauri can send them back as structured error objects.

/// Return all configured accounts.
pub fn get_accounts(cache: &Cache) -> Result<Vec<Account>, UnkaiError> {
    account_store::load_accounts(cache)
}

/// Add a new email account and store its password in the OS keychain.
///
/// The frontend sends an `Account` object plus a `password`. The account
/// metadata lands in the encrypted SQLite cache; the password goes to
/// the OS keychain. Separating them keeps secrets off disk and lets the
/// `accounts` table be inspected without exposing credentials.
pub fn add_account(
    account: Account,
    password: String,
    cache: &Cache,
    notify: &SettingsSyncNotify,
) -> Result<(), UnkaiError> {
    credentials::store_imap_password(&account.id, &password)?;
    account_store::add_account(cache, account)?;
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
pub fn remove_account(
    id: String,
    cache: &Cache,
    notify: &SettingsSyncNotify,
) -> Result<(), UnkaiError> {
    credentials::delete_imap_password(&id)?;
    // Best-effort: PGP key + passphrase belong to this account too
    // (#57).  No-op when no key was imported — we still call the
    // deleter because the keychain getter is what tells us the
    // entry exists, and "is there a key?" isn't a question we want
    // to answer just to pick between two cleanup paths.
    if let Err(e) = credentials::delete_pgp_private_key(&id) {
        tracing::warn!("failed to delete PGP private key for account '{id}': {e}");
    }
    if let Err(e) = credentials::delete_pgp_passphrase(&id) {
        tracing::warn!("failed to delete PGP passphrase for account '{id}': {e}");
    }
    // Best-effort: a failure here leaves orphaned cache rows but doesn't
    // block account removal. Log and continue.
    if let Err(e) = cache.wipe_account(&id) {
        tracing::warn!("failed to wipe cache for account '{id}': {e}");
    }
    account_store::remove_account(cache, &id)?;
    notify.0.notify_one();
    Ok(())
}

/// Update an existing account's settings.
pub fn update_account(
    account: Account,
    cache: &Cache,
    notify: &SettingsSyncNotify,
) -> Result<(), UnkaiError> {
    account_store::update_account(cache, account)?;
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
pub fn set_account_password(id: String, password: String) -> Result<(), UnkaiError> {
    if password.is_empty() {
        return Err(UnkaiError::Other("password must not be empty".into()));
    }
    credentials::store_imap_password(&id, &password)
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
pub async fn discover_account_settings(
    email: String,
) -> Result<Option<unkai_discovery::DiscoveredAccount>, UnkaiError> {
    match unkai_discovery::discover(&email).await {
        Ok(found) => Ok(Some(found)),
        Err(unkai_discovery::DiscoveryError::NotFound) => Ok(None),
        Err(unkai_discovery::DiscoveryError::Parse(msg)) => Err(UnkaiError::Other(msg)),
        Err(unkai_discovery::DiscoveryError::Network(msg)) => Err(UnkaiError::Network(msg)),
    }
}

/// The hardcoded provider table for the wizard's pick-list (#413).
/// Pure data, no I/O — safe to call any time.
pub fn list_provider_presets() -> Vec<unkai_discovery::ProviderPreset> {
    unkai_discovery::presets::all()
}

/// One cert in a probed chain — DER bytes plus its SHA-256
/// fingerprint formatted for display. The frontend uses `der` to
/// build a `TrustedCert` entry and `sha256` to render the
/// "compare this against your server" prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbedCertEntry {
    pub der: Vec<u8>,
    pub sha256: String,
}

/// Shape returned to the UI by [`probe_server_certificate`]. The
/// full chain (leaf first, then intermediates) is round-tripped
/// back so the UI can trust every cert the server presented — not
/// just the leaf. This survives chain reordering and reissues of
/// the leaf under the same intermediate without a re-prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbedCert {
    /// Probed certificates in handshake order (leaf at index 0).
    pub chain: Vec<ProbedCertEntry>,
    pub host: String,
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
pub async fn probe_server_certificate(host: String, port: u16) -> Result<ProbedCert, UnkaiError> {
    let chain_der = unkai_imap::probe_server_certificate(&host, port).await?;
    let chain = chain_der
        .into_iter()
        .map(|der| {
            let sha256 = unkai_core::tls::fingerprint_sha256(&der);
            ProbedCertEntry { der, sha256 }
        })
        .collect();
    Ok(ProbedCert { chain, host })
}

/// Validate IMAP credentials by actually logging in.
///
/// The setup wizard calls this before it asks the store to persist the
/// account — an early TCP/TLS/LOGIN round-trip surfaces wrong hostnames,
/// wrong ports, and bad passwords as a structured `UnkaiError` with a
/// specific variant (`Network`, `Auth`, `Protocol`) so the UI can phrase
/// the failure clearly instead of saving a dead account and confusing
/// the user on first fetch.
///
/// The session is immediately torn down — this is a probe, not a real
/// fetch; nothing is cached.
pub async fn test_connection(
    host: String,
    port: u16,
    username: String,
    password: String,
    trusted_certs: Option<Vec<unkai_core::models::TrustedCert>>,
) -> Result<String, UnkaiError> {
    tracing::info!("Testing IMAP connection to {host}:{port} as {username}");
    let trusted = trusted_certs.unwrap_or_default();
    let client = ImapClient::connect(&host, port, &username, &password, &trusted).await?;
    let _ = client.logout().await;
    Ok(format!("IMAP login to {host}:{port} succeeded"))
}

// ── JMAP commands ──────────────────────────────────────────────────

/// Test a JMAP connection by performing session discovery.
///
/// Similar to `test_connection` for IMAP — the setup wizard uses this
/// to verify JMAP credentials before saving the account.
pub async fn test_jmap_connection(
    jmap_url: String,
    username: String,
    password: String,
) -> Result<String, UnkaiError> {
    tracing::info!("Testing JMAP connection to {jmap_url} as {username}");
    JmapClient::test(&jmap_url, &username, &password).await
}

/// Probe whether a server supports JMAP by trying `.well-known/jmap`.
///
/// Returns the JMAP base URL if discovered, or `None` if the server
/// doesn't support JMAP. This is a best-effort probe — it's fine to
/// fall back to IMAP if this fails.
pub async fn detect_jmap(host: String) -> Result<Option<String>, UnkaiError> {
    // Try HTTPS first (standard), then HTTP as fallback.
    for scheme in &["https", "http"] {
        let url = format!("{scheme}://{host}/.well-known/jmap");
        tracing::debug!("Probing JMAP at {url}");

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| UnkaiError::Network(format!("HTTP client error: {e}")))?;

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

//! Credential storage via the OS keychain.
//!
//! Passwords are never written to `accounts.json` — they live in:
//!   - **Windows**: Credential Manager
//!   - **macOS**: Keychain
//!   - **Linux**: Secret Service (GNOME Keyring, KWallet, ...)
//!
//! We key each secret by `(service, account_id)` where `service` is a
//! constant string and `account_id` is the account's UUID. Using the UUID
//! means the keychain entry is stable even if the user changes their email
//! address, and avoids collisions when two accounts share an email.

use keyring::Entry;
use tracing::{debug, info};
use unkai_core::UnkaiError;

/// Keychain service name for IMAP passwords.
/// Shown to the user in Credential Manager / Keychain Access as the "service".
const IMAP_SERVICE: &str = "unkai-mail-imap";

/// Keychain service name for Nextcloud app passwords.
/// Separate from IMAP so revoking one can't touch the other — removing
/// a mail account should not log the user out of their Nextcloud, and
/// vice versa.
const NEXTCLOUD_SERVICE: &str = "unkai-mail-nextcloud";

fn entry(account_id: &str) -> Result<Entry, UnkaiError> {
    Entry::new(IMAP_SERVICE, account_id)
        .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))
}

fn nc_entry(account_id: &str) -> Result<Entry, UnkaiError> {
    Entry::new(NEXTCLOUD_SERVICE, account_id)
        .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))
}

/// Store (or overwrite) the IMAP password for an account.
pub fn store_imap_password(account_id: &str, password: &str) -> Result<(), UnkaiError> {
    entry(account_id)?
        .set_password(password)
        .map_err(|e| UnkaiError::Storage(format!("failed to store password: {e}")))?;
    info!("Stored IMAP password for account '{account_id}' in OS keychain");
    Ok(())
}

/// Retrieve the IMAP password for an account.
pub fn get_imap_password(account_id: &str) -> Result<String, UnkaiError> {
    entry(account_id)?
        .get_password()
        .map_err(|e| UnkaiError::Auth(format!("no password found for account '{account_id}': {e}")))
}

/// Remove the IMAP password for an account. Silently succeeds if the entry
/// doesn't exist — useful during account removal where we can't be sure
/// whether a password was ever stored.
pub fn delete_imap_password(account_id: &str) -> Result<(), UnkaiError> {
    match entry(account_id)?.delete_credential() {
        Ok(()) => {
            info!("Deleted IMAP password for account '{account_id}'");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            debug!("No password to delete for account '{account_id}' (ok)");
            Ok(())
        }
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to delete password: {e}"
        ))),
    }
}

// ── Nextcloud app password ──────────────────────────────────────
//
// Symmetric API to the IMAP functions above. Kept as separate
// functions (rather than a generic one parameterised by service) so
// the call sites read clearly — you can see at a glance which kind
// of secret a caller is reaching for.

/// Store (or overwrite) the Nextcloud app password for a connection.
pub fn store_nextcloud_password(nc_id: &str, app_password: &str) -> Result<(), UnkaiError> {
    nc_entry(nc_id)?
        .set_password(app_password)
        .map_err(|e| UnkaiError::Storage(format!("failed to store NC password: {e}")))?;
    info!("Stored Nextcloud app password for connection '{nc_id}'");
    Ok(())
}

/// Retrieve the Nextcloud app password for a connection.
pub fn get_nextcloud_password(nc_id: &str) -> Result<String, UnkaiError> {
    nc_entry(nc_id)?.get_password().map_err(|e| {
        UnkaiError::Auth(format!(
            "no Nextcloud password found for connection '{nc_id}': {e}"
        ))
    })
}

/// Remove the Nextcloud app password for a connection; no-op if missing.
pub fn delete_nextcloud_password(nc_id: &str) -> Result<(), UnkaiError> {
    match nc_entry(nc_id)?.delete_credential() {
        Ok(()) => {
            info!("Deleted Nextcloud password for connection '{nc_id}'");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            debug!("No NC password to delete for '{nc_id}' (ok)");
            Ok(())
        }
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to delete NC password: {e}"
        ))),
    }
}

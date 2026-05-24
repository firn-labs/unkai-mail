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

/// Keychain service name for the armored OpenPGP private key (#57).
/// Kept on its own service so revoking a PGP key doesn't touch the
/// IMAP password and vice versa — losing one shouldn't lock the user
/// out of the other.
const PGP_KEY_SERVICE: &str = "unkai-mail-pgp-private-key";

/// Keychain service name for the passphrase that unlocks the OpenPGP
/// private key (#57).  Stored separately from the key itself so the
/// key can be exported/backed up without leaking the passphrase, and
/// so a future "remember passphrase for session" flow can flip a
/// single keychain entry without touching the key blob.
const PGP_PASSPHRASE_SERVICE: &str = "unkai-mail-pgp-passphrase";

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

// ── OpenPGP private key + passphrase (#57) ─────────────────────
//
// Same shape as the IMAP / Nextcloud password APIs above.  Two
// distinct keychain entries per account: one for the armored
// `-----BEGIN PGP PRIVATE KEY BLOCK-----` ASCII (often several KiB),
// one for the passphrase that unlocks it (typically a short string).
//
// Keying by `account_id` so a user with two accounts (work / personal)
// can hold a separate signing key per account — matching how IMAP
// passwords already work.  The keychain is the only place the
// cleartext key material ever lives; the SQLCipher cache only carries
// the *fingerprint* (a public identifier) for UI display.

fn pgp_key_entry(account_id: &str) -> Result<Entry, UnkaiError> {
    Entry::new(PGP_KEY_SERVICE, account_id)
        .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))
}

fn pgp_pw_entry(account_id: &str) -> Result<Entry, UnkaiError> {
    Entry::new(PGP_PASSPHRASE_SERVICE, account_id)
        .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))
}

/// Store (or overwrite) the armored OpenPGP private key for an account.
///
/// `armored_key` is the full ASCII armored form starting with
/// `-----BEGIN PGP PRIVATE KEY BLOCK-----`.  Callers should validate
/// the key parses (via `unkai_crypto::parse_private_key`) *before*
/// storing — we don't re-validate here because the keychain backend
/// treats the value as an opaque string.
pub fn store_pgp_private_key(account_id: &str, armored_key: &str) -> Result<(), UnkaiError> {
    pgp_key_entry(account_id)?
        .set_password(armored_key)
        .map_err(|e| UnkaiError::Storage(format!("failed to store PGP private key: {e}")))?;
    info!("Stored PGP private key for account '{account_id}' in OS keychain");
    Ok(())
}

/// Retrieve the armored OpenPGP private key for an account.  Returns
/// `UnkaiError::Auth` when the entry doesn't exist — same shape as
/// the IMAP password getter so the IPC layer can route the missing
/// case to a "set up encryption" prompt rather than a generic toast.
pub fn get_pgp_private_key(account_id: &str) -> Result<String, UnkaiError> {
    pgp_key_entry(account_id)?.get_password().map_err(|e| {
        UnkaiError::Auth(format!(
            "no PGP private key found for account '{account_id}': {e}"
        ))
    })
}

/// Remove the PGP private key for an account; no-op if missing.  Always
/// called from the account-removal path so revoking the key doesn't
/// leave orphaned credentials in the OS keychain.
pub fn delete_pgp_private_key(account_id: &str) -> Result<(), UnkaiError> {
    match pgp_key_entry(account_id)?.delete_credential() {
        Ok(()) => {
            info!("Deleted PGP private key for account '{account_id}'");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            debug!("No PGP private key to delete for account '{account_id}' (ok)");
            Ok(())
        }
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to delete PGP private key: {e}"
        ))),
    }
}

/// Store (or overwrite) the passphrase that unlocks the PGP private
/// key for an account.  Pass an empty string for an unprotected key.
///
/// Per the "re-prompt on every operation" decision in #57, the
/// passphrase is *not* automatically replayed on every send/decrypt —
/// the IPC layer reads it at use time so a leak of the cache doesn't
/// also expose the unlocking secret.
pub fn store_pgp_passphrase(account_id: &str, passphrase: &str) -> Result<(), UnkaiError> {
    pgp_pw_entry(account_id)?
        .set_password(passphrase)
        .map_err(|e| UnkaiError::Storage(format!("failed to store PGP passphrase: {e}")))?;
    info!("Stored PGP passphrase for account '{account_id}' in OS keychain");
    Ok(())
}

/// Retrieve the PGP passphrase for an account.
pub fn get_pgp_passphrase(account_id: &str) -> Result<String, UnkaiError> {
    pgp_pw_entry(account_id)?.get_password().map_err(|e| {
        UnkaiError::Auth(format!(
            "no PGP passphrase found for account '{account_id}': {e}"
        ))
    })
}

/// Non-erroring sibling of [`get_pgp_passphrase`] — `Ok(true)` when a
/// passphrase is stored for the account, `Ok(false)` when it isn't,
/// and `Err` only when the keychain itself is misbehaving (the OS
/// service is down, the credential entry is malformed, …).  Used by
/// the per-account "Unlock automatically" toggle in Encryption
/// Settings so the renderer can render the on/off state without
/// having to treat a missing-entry error as the no-op it really is
/// (#341).
pub fn has_pgp_passphrase(account_id: &str) -> Result<bool, UnkaiError> {
    match pgp_pw_entry(account_id)?.get_password() {
        Ok(_) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to query PGP passphrase: {e}"
        ))),
    }
}

/// Remove the PGP passphrase for an account; no-op if missing.
pub fn delete_pgp_passphrase(account_id: &str) -> Result<(), UnkaiError> {
    match pgp_pw_entry(account_id)?.delete_credential() {
        Ok(()) => {
            info!("Deleted PGP passphrase for account '{account_id}'");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            debug!("No PGP passphrase to delete for account '{account_id}' (ok)");
            Ok(())
        }
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to delete PGP passphrase: {e}"
        ))),
    }
}

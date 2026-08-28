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

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
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

/// Keychain service name for the user's own S/MIME identity (#338) —
/// the PKCS#12 (`.p12`) bundle carrying the leaf certificate, private
/// key, and any intermediate CA chain.  X.509 counterpart to
/// `PGP_KEY_SERVICE`; kept on its own service so revoking an S/MIME
/// identity can't touch the OpenPGP key or the IMAP password.
///
/// The `.p12` is binary, but the keychain backends store strings, so we
/// base64-encode the bytes on the way in and decode them on the way out
/// — the encoding is an implementation detail of these helpers and
/// never leaks to callers, which deal in raw `&[u8]` / `Vec<u8>`.
const SMIME_CERT_SERVICE: &str = "unkai-mail-smime-private-cert";

/// Keychain service name for the passphrase that unlocks the S/MIME
/// `.p12` (#338).  X.509 counterpart to `PGP_PASSPHRASE_SERVICE`,
/// stored separately from the bundle for the same reasons: the `.p12`
/// can be backed up without leaking the passphrase, and the
/// per-account "Unlock automatically" toggle can flip this single
/// entry without touching the cert blob.
const SMIME_PASSPHRASE_SERVICE: &str = "unkai-mail-smime-passphrase";

/// Keychain service name for the MCP server's bearer token (#438).
/// Own service so revoking the AI integration can't touch mail or
/// Nextcloud credentials and vice versa.  One MCP server runs per
/// *profile* (#533), so entries are keyed `bearer-token:<profile-id>`
/// — one token per profile, otherwise a single token would unlock
/// every profile's data and defeat the isolation.
///
/// The token is the only secret in the MCP surface and the
/// keychain is the only place it ever persists: it is never
/// written into `AppSettings` and therefore never enters the
/// Nextcloud settings-sync bundle.
const MCP_TOKEN_SERVICE: &str = "unkai-mail-mcp";

/// Keychain "user" prefix for the per-profile MCP bearer token.
/// The pre-profile singleton lived under the bare prefix; see
/// [`migrate_legacy_mcp_token`].
const MCP_TOKEN_USER: &str = "bearer-token";

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

// ── MCP server bearer token (#438) ─────────────────────────────
//
// Same shape as the password APIs above, keyed by *profile* id
// (#533): each profile's MCP server has its own token, so no
// token grants access across profile boundaries.

fn mcp_token_entry(profile_id: &str) -> Result<Entry, UnkaiError> {
    Entry::new(MCP_TOKEN_SERVICE, &format!("{MCP_TOKEN_USER}:{profile_id}"))
        .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))
}

/// Store (or overwrite) a profile's MCP bearer token.  Generating
/// a new token invalidates the old one by definition — there is
/// exactly one valid token per profile at a time.
pub fn store_mcp_token(profile_id: &str, token: &str) -> Result<(), UnkaiError> {
    mcp_token_entry(profile_id)?
        .set_password(token)
        .map_err(|e| UnkaiError::Storage(format!("failed to store MCP token: {e}")))?;
    info!("Stored MCP bearer token in OS keychain");
    Ok(())
}

/// Retrieve a profile's MCP bearer token, or `Ok(None)` when the
/// user has never generated one (or has revoked it).  A missing
/// token is a normal state — the server still runs but rejects
/// every request with 401 until a token exists — so unlike the
/// password getters this doesn't map "no entry" to an error.
pub fn get_mcp_token(profile_id: &str) -> Result<Option<String>, UnkaiError> {
    match mcp_token_entry(profile_id)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to read MCP token: {e}"
        ))),
    }
}

/// Non-erroring existence probe for the settings UI — same shape
/// as [`has_pgp_passphrase`].
pub fn has_mcp_token(profile_id: &str) -> Result<bool, UnkaiError> {
    Ok(get_mcp_token(profile_id)?.is_some())
}

/// Move the pre-profile singleton `bearer-token` entry to this
/// profile's `bearer-token:<id>` account (#533) — the MCP twin of
/// [`crate::cache::key::migrate_legacy_master_key`].  Only called
/// when the registry holds exactly one profile, so the target is
/// unambiguous.  No-ops on a fresh install and on an already-
/// migrated one; ordering is write-new → read-back-verify →
/// delete-old so no failure mode drops the user's token.
pub fn migrate_legacy_mcp_token(profile_id: &str) -> Result<(), UnkaiError> {
    let legacy = Entry::new(MCP_TOKEN_SERVICE, MCP_TOKEN_USER)
        .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))?;
    let token = match legacy.get_password() {
        Ok(t) => t,
        Err(keyring::Error::NoEntry) => return Ok(()), // fresh, or already migrated
        Err(e) => {
            return Err(UnkaiError::Storage(format!(
                "failed to read legacy MCP token: {e}"
            )));
        }
    };
    match get_mcp_token(profile_id)? {
        None => {
            store_mcp_token(profile_id, &token)?;
            if get_mcp_token(profile_id)?.as_deref() != Some(token.as_str()) {
                return Err(UnkaiError::Storage(
                    "MCP token migration verification failed — keeping the legacy entry".into(),
                ));
            }
            if let Err(e) = legacy.delete_credential() {
                // Both copies exist and match; the next boot's
                // re-run retries the delete.  Not fatal.
                tracing::warn!("could not delete legacy MCP token entry: {e}");
            }
            info!("Migrated MCP bearer token to its per-profile entry");
            Ok(())
        }
        // Crash leftover from a previous run that wrote the new
        // entry but not the delete: the per-profile token wins,
        // just retry clearing the legacy copy.
        Some(_) => {
            if let Err(e) = legacy.delete_credential() {
                tracing::warn!("could not delete legacy MCP token entry: {e}");
            }
            Ok(())
        }
    }
}

/// Remove the MCP bearer token; no-op if missing.  Revoking cuts
/// off every connected client immediately (the server compares
/// against the in-memory copy the caller clears alongside this).
pub fn delete_mcp_token(profile_id: &str) -> Result<(), UnkaiError> {
    match mcp_token_entry(profile_id)?.delete_credential() {
        Ok(()) => {
            info!("Deleted MCP bearer token");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            debug!("No MCP token to delete (ok)");
            Ok(())
        }
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to delete MCP token: {e}"
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

// ── S/MIME identity (.p12 bundle) + passphrase (#338) ──────────
//
// X.509 counterpart to the OpenPGP private-key / passphrase APIs
// above.  Two distinct keychain entries per account: one for the
// PKCS#12 bundle (leaf cert + private key + optional chain, often a
// few KiB, base64-encoded because `.p12` is binary and the keychain
// holds strings), one for the passphrase that unlocks it.
//
// Keying by `account_id` so a user with two accounts (work / personal)
// can hold a separate S/MIME identity per account — matching how the
// IMAP password and OpenPGP key already work.  The keychain is the
// only place the private-key material ever lives; the SQLCipher cache
// only carries the *fingerprint* (a public identifier) for UI display.

fn smime_cert_entry(account_id: &str) -> Result<Entry, UnkaiError> {
    Entry::new(SMIME_CERT_SERVICE, account_id)
        .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))
}

fn smime_pw_entry(account_id: &str) -> Result<Entry, UnkaiError> {
    Entry::new(SMIME_PASSPHRASE_SERVICE, account_id)
        .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))
}

/// Store (or overwrite) the user's S/MIME PKCS#12 bundle for an
/// account.  `p12_der` is the raw binary `.p12` as the user imported
/// it; we base64-encode it for the (string-only) keychain backend.
///
/// Callers should validate the bundle parses (via
/// `unkai_crypto::parse_pkcs12`) with the supplied passphrase *before*
/// storing — we don't re-validate here because the keychain backend
/// treats the value as an opaque string.
pub fn store_smime_private_cert(account_id: &str, p12_der: &[u8]) -> Result<(), UnkaiError> {
    let encoded = B64.encode(p12_der);
    smime_cert_entry(account_id)?
        .set_password(&encoded)
        .map_err(|e| UnkaiError::Storage(format!("failed to store S/MIME identity: {e}")))?;
    info!("Stored S/MIME identity for account '{account_id}' in OS keychain");
    Ok(())
}

/// Retrieve the user's S/MIME PKCS#12 bundle for an account, decoded
/// back to raw `.p12` bytes.  Returns `UnkaiError::Auth` when the entry
/// doesn't exist — same shape as the PGP private-key getter so the IPC
/// layer can route the missing case to a "set up encryption" prompt.
pub fn get_smime_private_cert(account_id: &str) -> Result<Vec<u8>, UnkaiError> {
    let encoded = smime_cert_entry(account_id)?.get_password().map_err(|e| {
        UnkaiError::Auth(format!(
            "no S/MIME identity found for account '{account_id}': {e}"
        ))
    })?;
    B64.decode(encoded.as_bytes())
        .map_err(|e| UnkaiError::Storage(format!("failed to decode stored S/MIME identity: {e}")))
}

/// Remove the S/MIME identity for an account; no-op if missing.  Always
/// called from the account-removal path so revoking the identity
/// doesn't leave orphaned credentials in the OS keychain.
pub fn delete_smime_private_cert(account_id: &str) -> Result<(), UnkaiError> {
    match smime_cert_entry(account_id)?.delete_credential() {
        Ok(()) => {
            info!("Deleted S/MIME identity for account '{account_id}'");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            debug!("No S/MIME identity to delete for account '{account_id}' (ok)");
            Ok(())
        }
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to delete S/MIME identity: {e}"
        ))),
    }
}

/// Store (or overwrite) the passphrase that unlocks the S/MIME `.p12`
/// for an account.  Pass an empty string for an unprotected bundle.
///
/// As with PGP, the passphrase is *not* automatically replayed on
/// every send / decrypt — the IPC layer reads it at use time so a leak
/// of the cache doesn't also expose the unlocking secret.  Only the
/// per-account "Unlock automatically" opt-in writes it here.
pub fn store_smime_passphrase(account_id: &str, passphrase: &str) -> Result<(), UnkaiError> {
    smime_pw_entry(account_id)?
        .set_password(passphrase)
        .map_err(|e| UnkaiError::Storage(format!("failed to store S/MIME passphrase: {e}")))?;
    info!("Stored S/MIME passphrase for account '{account_id}' in OS keychain");
    Ok(())
}

/// Retrieve the S/MIME passphrase for an account.
pub fn get_smime_passphrase(account_id: &str) -> Result<String, UnkaiError> {
    smime_pw_entry(account_id)?.get_password().map_err(|e| {
        UnkaiError::Auth(format!(
            "no S/MIME passphrase found for account '{account_id}': {e}"
        ))
    })
}

/// Non-erroring sibling of [`get_smime_passphrase`] — `Ok(true)` when a
/// passphrase is stored for the account, `Ok(false)` when it isn't, and
/// `Err` only when the keychain itself is misbehaving.  Drives the
/// per-account "Unlock automatically" toggle in the S/MIME settings so
/// the renderer can render the on/off state without treating a
/// missing-entry error as the no-op it really is.
pub fn has_smime_passphrase(account_id: &str) -> Result<bool, UnkaiError> {
    match smime_pw_entry(account_id)?.get_password() {
        Ok(_) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to query S/MIME passphrase: {e}"
        ))),
    }
}

/// Remove the S/MIME passphrase for an account; no-op if missing.
pub fn delete_smime_passphrase(account_id: &str) -> Result<(), UnkaiError> {
    match smime_pw_entry(account_id)?.delete_credential() {
        Ok(()) => {
            info!("Deleted S/MIME passphrase for account '{account_id}'");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            debug!("No S/MIME passphrase to delete for account '{account_id}' (ok)");
            Ok(())
        }
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to delete S/MIME passphrase: {e}"
        ))),
    }
}

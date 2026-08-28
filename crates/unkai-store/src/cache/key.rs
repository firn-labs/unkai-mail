//! Master key management for the SQLCipher-encrypted mail cache.
//!
//! # What this solves
//!
//! The cache DB is encrypted at rest with AES-256 via SQLCipher. For that
//! to work, every connection has to be unlocked with a key before it can
//! read or write. We don't want to prompt the user for a passphrase every
//! time the app starts — so instead we generate a high-entropy random key
//! once and store it in the OS keychain, which is already protected by
//! the user's login session.
//!
//! # Threat model
//!
//! What this protects against:
//! - Another user on the same machine copying `cache.db` — the key lives
//!   in *their* keychain, not the file, so they see gibberish.
//! - A stolen laptop where the disk is readable but the account is
//!   locked — keychain entries are gated on the OS user session.
//! - Backup drives, cloud sync, malware exfiltrating files from the app
//!   data directory.
//!
//! What this does *not* protect against:
//! - Malware running as the current user — it can ask the keychain for
//!   the key just like we do. (Folder-level encryption with a separate
//!   passphrase would cover this, and is planned as a follow-up.)
//! - A forensic image of RAM while the app is running.
//!
//! # Key format
//!
//! 32 random bytes (256 bits), hex-encoded (64 chars) for keychain
//! storage and for SQLCipher's `PRAGMA key = "x'<hex>'"` syntax.
//! SQLCipher treats a hex literal of the right length as a raw key and
//! skips PBKDF2 derivation, which is both faster and — since we already
//! have cryptographic randomness — just as secure as a derived key.

use getrandom::fill;
use keyring::Entry;
use tracing::{debug, info};
use unkai_core::UnkaiError;

use crate::fido::{KeychainEnvelope, WrappedKey, parse_envelope, serialize_envelope};

/// Keychain service name for the DB master key. Separate from the IMAP
/// service so the entry is easy to spot in Credential Manager / Keychain
/// Access, and so revoking the DB key can't touch account passwords.
const DB_SERVICE: &str = "unkai-mail-db";

/// Account name the pre-profile builds used.  There used to be
/// exactly one master key per install; profiles (#531) key the
/// account per profile instead — this name survives only so
/// [`migrate_legacy_master_key`] can find and move the old entry.
const LEGACY_DB_ACCOUNT: &str = "master-key";

/// Byte length of the raw AES-256 key. Hex-encoded this becomes 64 chars.
const KEY_LEN: usize = 32;

/// Keychain account name for one profile's master key.  Each
/// profile encrypts its own `cache.db` with its own key, so the
/// account carries the profile id: `master-key:<profile-id>`.
fn db_account(profile_id: &str) -> String {
    format!("master-key:{profile_id}")
}

fn entry(profile_id: &str) -> Result<Entry, UnkaiError> {
    Entry::new(DB_SERVICE, &db_account(profile_id))
        .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))
}

/// Fetch the master key, generating and persisting a new one on first run.
///
/// Returned as a 64-character lowercase hex string, ready to be embedded
/// directly into a `PRAGMA key = "x'<hex>'"` statement.
///
/// Reads the keychain envelope (#164).  In **plain mode** (or the
/// pre-FIDO format with a bare hex string), the envelope's
/// `plain_key` is the answer.  When the envelope is in FIDO-only
/// mode (no `plain_key`), the caller must instead unwrap one of the
/// stored wraps via [`unlock_with_prf`].
pub fn get_or_create_master_key(profile_id: &str) -> Result<String, UnkaiError> {
    let entry = entry(profile_id)?;
    match entry.get_password() {
        Ok(raw) => {
            let env = parse_envelope(&raw)?;
            if let Some(hex) = env.plain_key.as_deref() {
                if hex.len() != KEY_LEN * 2 {
                    return Err(UnkaiError::Storage(format!(
                        "unexpected master key length: {} chars (expected {})",
                        hex.len(),
                        KEY_LEN * 2
                    )));
                }
                debug!("Loaded existing DB master key from keychain");
                Ok(hex.to_string())
            } else {
                Err(UnkaiError::Auth(
                    "Database is in FIDO-only mode — call unlock_with_prf first".into(),
                ))
            }
        }
        Err(keyring::Error::NoEntry) => {
            info!("No DB master key in keychain — generating a new one");
            let hex_key = generate_hex_key()?;
            let env = KeychainEnvelope::new_plain(hex_key.clone());
            entry
                .set_password(&serialize_envelope(&env)?)
                .map_err(|e| UnkaiError::Storage(format!("failed to store master key: {e}")))?;
            Ok(hex_key)
        }
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to read master key: {e}"
        ))),
    }
}

/// Read the raw keychain envelope.  Used by FIDO management
/// commands (list / remove credentials) and by the boot path that
/// decides whether to show the lock screen.
pub fn load_envelope(profile_id: &str) -> Result<KeychainEnvelope, UnkaiError> {
    let entry = entry(profile_id)?;
    match entry.get_password() {
        Ok(raw) => parse_envelope(&raw),
        Err(keyring::Error::NoEntry) => Ok(KeychainEnvelope {
            version: 1,
            plain_key: None,
            wraps: Vec::new(),
            wipe_on_failure: false,
            max_unlock_attempts: None,
            failed_attempts: 0,
            integrity_mac: None,
        }),
        Err(e) => Err(UnkaiError::Storage(format!(
            "failed to read master key: {e}"
        ))),
    }
}

/// Persist a mutated envelope back to the keychain.  Recomputes
/// the integrity MAC from the (current) field values so any
/// later in-place edit of the JSON in the keychain breaks the
/// MAC and trips `verify_envelope_mac` on next load.
pub fn save_envelope(profile_id: &str, env: &KeychainEnvelope) -> Result<(), UnkaiError> {
    use crate::fido;
    let mut signed = env.clone();
    signed.integrity_mac = None;
    signed.integrity_mac = Some(fido::compute_envelope_mac(&signed)?);
    let entry = entry(profile_id)?;
    entry
        .set_password(&serialize_envelope(&signed)?)
        .map_err(|e| UnkaiError::Storage(format!("failed to store master key: {e}")))
}

/// Was the loaded envelope tampered with?  True only when an
/// `integrity_mac` is present *and* doesn't match.  A missing
/// MAC (legacy envelope or first save) returns `false` so we
/// don't wipe the user's data on the upgrade boundary — the
/// next `save_envelope` will write a fresh MAC.
pub fn envelope_tampered(env: &KeychainEnvelope) -> bool {
    if env.integrity_mac.is_none() {
        return false;
    }
    match crate::fido::verify_envelope_mac(env) {
        Ok(valid) => !valid,
        Err(e) => {
            tracing::warn!("envelope MAC verify errored: {e}");
            true
        }
    }
}

/// Append (or replace) a wrap in the envelope, keyed on
/// `credential_id` so re-enrolling the same authenticator just
/// updates the existing entry.
pub fn add_wrap(profile_id: &str, new: WrappedKey) -> Result<(), UnkaiError> {
    let mut env = load_envelope(profile_id)?;
    env.wraps.retain(|w| w.credential_id != new.credential_id);
    env.wraps.push(new);
    save_envelope(profile_id, &env)
}

/// Remove a wrap by credential id.  Returns whether something was
/// actually removed (caller may want to surface "no such credential"
/// to the user).
pub fn remove_wrap(profile_id: &str, credential_id_b64: &str) -> Result<bool, UnkaiError> {
    let mut env = load_envelope(profile_id)?;
    let before = env.wraps.len();
    env.wraps.retain(|w| w.credential_id != credential_id_b64);
    let removed = env.wraps.len() < before;
    if removed {
        save_envelope(profile_id, &env)?;
    }
    Ok(removed)
}

fn generate_hex_key() -> Result<String, UnkaiError> {
    let mut buf = [0u8; KEY_LEN];
    fill(&mut buf).map_err(|e| UnkaiError::Storage(format!("RNG failed: {e}")))?;
    Ok(hex::encode(buf))
}

// ── Legacy-entry migration (#531) ─────────────────────────────
//
// Pre-profile builds stored the envelope under the singleton
// account `master-key`.  Profiles key it per profile, so the boot
// migration moves the raw envelope string — wraps, MAC and all —
// to `master-key:<id>`.  The MAC covers only the envelope's own
// JSON (see `fido::compute_envelope_mac`), not the keychain
// account name, so moving the string verbatim keeps it valid and
// a locked (FIDO-only) vault boots into the locked state exactly
// as before.

/// The minimal keychain surface the migration needs.  Abstracted
/// so the delete-ordering guarantees below are unit-testable
/// without touching a real OS keychain.
trait SecretSlot {
    /// `Ok(None)` when the slot has no entry.
    fn read(&self) -> Result<Option<String>, UnkaiError>;
    fn write(&self, value: &str) -> Result<(), UnkaiError>;
    fn delete(&self) -> Result<(), UnkaiError>;
}

/// One account under the `unkai-mail-db` keychain service.
struct KeyringSlot {
    account: String,
}

impl SecretSlot for KeyringSlot {
    fn read(&self) -> Result<Option<String>, UnkaiError> {
        let entry = Entry::new(DB_SERVICE, &self.account)
            .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))?;
        match entry.get_password() {
            Ok(raw) => Ok(Some(raw)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(UnkaiError::Storage(format!(
                "failed to read keychain entry '{}': {e}",
                self.account
            ))),
        }
    }

    fn write(&self, value: &str) -> Result<(), UnkaiError> {
        let entry = Entry::new(DB_SERVICE, &self.account)
            .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))?;
        entry.set_password(value).map_err(|e| {
            UnkaiError::Storage(format!(
                "failed to write keychain entry '{}': {e}",
                self.account
            ))
        })
    }

    fn delete(&self) -> Result<(), UnkaiError> {
        let entry = Entry::new(DB_SERVICE, &self.account)
            .map_err(|e| UnkaiError::Storage(format!("keychain entry init failed: {e}")))?;
        entry.delete_credential().map_err(|e| {
            UnkaiError::Storage(format!(
                "failed to delete keychain entry '{}': {e}",
                self.account
            ))
        })
    }
}

/// Move the pre-profile singleton `master-key` entry to this
/// profile's `master-key:<id>` account.  No-ops on a fresh install
/// (no legacy entry) and on an already-migrated one.  Ordering is
/// write-new → read-back-verify → delete-old, so no failure mode
/// leaves the key with zero copies.
pub fn migrate_legacy_master_key(profile_id: &str) -> Result<(), UnkaiError> {
    let legacy = KeyringSlot {
        account: LEGACY_DB_ACCOUNT.into(),
    };
    let new = KeyringSlot {
        account: db_account(profile_id),
    };
    migrate_slot(&legacy, &new)
}

fn migrate_slot(legacy: &impl SecretSlot, new: &impl SecretSlot) -> Result<(), UnkaiError> {
    let Some(raw) = legacy.read()? else {
        return Ok(()); // fresh install, or already migrated
    };
    match new.read()? {
        None => {
            new.write(&raw)?;
            // Never trust the write blindly: only a successful
            // read-back of identical bytes releases the old copy.
            if new.read()?.as_deref() != Some(raw.as_str()) {
                return Err(UnkaiError::Storage(
                    "master-key migration verification failed — keeping the legacy entry".into(),
                ));
            }
            if let Err(e) = legacy.delete() {
                // Both copies exist and match; the next boot's
                // re-run retries the delete.  Not fatal.
                tracing::warn!("could not delete legacy master-key entry: {e}");
            }
            info!("Migrated keychain master key to its per-profile entry");
            Ok(())
        }
        // Crash leftover from a previous run that wrote the new
        // entry but died before deleting the old one.
        Some(existing) if existing == raw => {
            if let Err(e) = legacy.delete() {
                tracing::warn!("could not delete legacy master-key entry: {e}");
            }
            Ok(())
        }
        // Diverged copies should be impossible (the new entry is
        // only ever written from the legacy one).  Deleting either
        // could destroy the only key that decrypts the DB, so keep
        // both and keep using the per-profile one.
        Some(_) => {
            tracing::warn!(
                "legacy and per-profile master-key entries both exist and differ — \
                 keeping both, using the per-profile entry"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// In-memory slot; `fail_writes` simulates a keychain that
    /// accepts nothing.
    struct MemSlot {
        value: RefCell<Option<String>>,
        fail_writes: bool,
    }

    impl MemSlot {
        fn holding(v: Option<&str>) -> Self {
            Self {
                value: RefCell::new(v.map(String::from)),
                fail_writes: false,
            }
        }
    }

    impl SecretSlot for MemSlot {
        fn read(&self) -> Result<Option<String>, UnkaiError> {
            Ok(self.value.borrow().clone())
        }
        fn write(&self, value: &str) -> Result<(), UnkaiError> {
            if self.fail_writes {
                return Err(UnkaiError::Storage("simulated write failure".into()));
            }
            *self.value.borrow_mut() = Some(value.to_string());
            Ok(())
        }
        fn delete(&self) -> Result<(), UnkaiError> {
            *self.value.borrow_mut() = None;
            Ok(())
        }
    }

    #[test]
    fn migrates_legacy_entry_and_deletes_it_after_verify() {
        let legacy = MemSlot::holding(Some("envelope-json"));
        let new = MemSlot::holding(None);
        migrate_slot(&legacy, &new).expect("migrate");
        assert_eq!(new.value.borrow().as_deref(), Some("envelope-json"));
        assert_eq!(*legacy.value.borrow(), None);
    }

    #[test]
    fn no_ops_without_a_legacy_entry() {
        let legacy = MemSlot::holding(None);
        let new = MemSlot::holding(None);
        migrate_slot(&legacy, &new).expect("migrate");
        assert_eq!(*new.value.borrow(), None);
    }

    #[test]
    fn failed_write_keeps_the_legacy_entry() {
        let legacy = MemSlot::holding(Some("envelope-json"));
        let new = MemSlot {
            value: RefCell::new(None),
            fail_writes: true,
        };
        migrate_slot(&legacy, &new).expect_err("write failure must surface");
        // The one property that may never break: the legacy copy
        // survives every failure mode.
        assert_eq!(legacy.value.borrow().as_deref(), Some("envelope-json"));
    }

    #[test]
    fn crash_leftover_with_matching_copies_finishes_the_delete() {
        let legacy = MemSlot::holding(Some("envelope-json"));
        let new = MemSlot::holding(Some("envelope-json"));
        migrate_slot(&legacy, &new).expect("migrate");
        assert_eq!(*legacy.value.borrow(), None);
        assert_eq!(new.value.borrow().as_deref(), Some("envelope-json"));
    }

    #[test]
    fn diverged_copies_are_both_kept() {
        let legacy = MemSlot::holding(Some("old"));
        let new = MemSlot::holding(Some("different"));
        migrate_slot(&legacy, &new).expect("migrate");
        assert_eq!(legacy.value.borrow().as_deref(), Some("old"));
        assert_eq!(new.value.borrow().as_deref(), Some("different"));
    }
}

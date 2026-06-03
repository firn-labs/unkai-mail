//! FIDO unlock for the SQLCipher master key (#164).
//!
//! # What this is
//!
//! The cache database is encrypted with a 32-byte AES-256 master
//! key.  Today that key sits in plaintext in the OS keychain — fine
//! against "someone copies the disk" but not against malware running
//! as the same OS user (which can ask the keychain itself for the
//! key).  This module adds an opt-in second layer: instead of the
//! plain key, the keychain holds an *envelope* containing zero or
//! more wraps, each one a copy of the master key sealed by a
//! per-credential PRF output that only a registered FIDO2
//! authenticator (USB key, Touch ID, Windows Hello, …) can produce.
//!
//! Without the credential, the wrap is opaque.  An attacker with full
//! keychain access still can't reach the master key without the
//! user touching their authenticator.
//!
//! # The PRF protocol
//!
//! WebAuthn's PRF extension (RFC-bound; backed by CTAP2's
//! `hmac-secret`) lets us evaluate
//!
//! ```text
//! prf_output = HMAC-SHA-256(per-credential-secret, salt)
//! ```
//!
//! The per-credential secret never leaves the authenticator.  Same
//! `(credential_id, salt)` always produces the same 32-byte output,
//! but only after the user authenticates.  We store `salt` per wrap
//! and treat the resulting `prf_output` as a key.
//!
//! # The wrap
//!
//! For each registered credential we draw a random 12-byte nonce and
//! seal the master key with AES-256-GCM under `prf_output`, with the
//! `credential_id` as additional authenticated data so a wrap can't
//! be confused for one belonging to a different credential.
//!
//! ```text
//! ciphertext, tag = AES-256-GCM(
//!     key   = prf_output,
//!     nonce = random,
//!     aad   = credential_id,
//!     msg   = master_key,
//! )
//! ```
//!
//! Each wrap independently encrypts the *same* master key, so any
//! one registered credential is enough to unlock.  Adding /
//! removing a key only mutates the wraps array — the master key
//! itself never changes (so the encrypted DB doesn't have to be
//! re-keyed).
//!
//! # Storage shape
//!
//! The `unkai-mail-db` keychain entry holds JSON:
//!
//! ```json
//! {
//!   "version": 1,
//!   "plain_key": "<64-char hex>",
//!   "wraps": [
//!     {
//!       "credential_id": "<base64>",
//!       "salt":          "<base64>",
//!       "label":         "YubiKey 5C",
//!       "nonce":         "<base64>",
//!       "ciphertext":    "<base64>",
//!       "created_at":    1735689600
//!     }
//!   ]
//! }
//! ```
//!
//! `plain_key` is present in plain mode and after enrolling the
//! first FIDO credential (Phase 1A keeps it for backwards-compat).
//! Phase 1B will null it once startup unlock through FIDO is
//! wired, leaving the wraps as the only path to the master key.
//!
//! Pre-#164 keychains hold the bare 64-char hex string instead of
//! JSON; `parse_envelope` migrates that to the new shape on first
//! read.

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use chrono::Utc;
use getrandom::fill;
use hmac::Hmac;
use pbkdf2::pbkdf2;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use unkai_core::UnkaiError;

/// Bytes in a master key (AES-256).
const MASTER_KEY_LEN: usize = 32;
/// Bytes in the PRF output WebAuthn returns to us (HMAC-SHA-256).
const PRF_LEN: usize = 32;
/// AES-GCM nonce length.
const NONCE_LEN: usize = 12;
/// Bytes in a wrap salt — the WebAuthn PRF eval input AND the
/// PBKDF2 salt for passphrase wraps.  Same value, different
/// derivation downstream.
const SALT_LEN: usize = 32;

/// PBKDF2-HMAC-SHA-256 iteration count for passphrase wraps.
/// OWASP's 2024 floor for SHA-256 is 600 000 — we round up to
/// match a Bitwarden-class baseline.  At ~1 ms per 100 000 iters
/// on modern CPUs this is ~7 ms total at unlock, imperceptible.
pub const PASSPHRASE_PBKDF2_ITERS: u32 = 720_000;

/// What kind of source produced the AES key that sealed a wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrapKind {
    /// FIDO2 WebAuthn PRF / hmac-secret extension.  The
    /// `credential_id` field identifies the authenticator and
    /// the `salt` is fed back to WebAuthn's `prf.eval.first` at
    /// unlock to reproduce the same 32-byte output.
    FidoPrf,
    /// PBKDF2-HMAC-SHA-256(passphrase, salt, iters).  Used for
    /// the recovery-passphrase fallback (so a lost hardware key
    /// isn't a permanent lockout) and as a development-only path
    /// on platforms whose WebAuthn implementation can't reach
    /// PRF / hmac-secret yet (Linux WebKitGTK < 2.46, …).
    Passphrase,
}

/// One sealed copy of the master key.  Bound to a single
/// authentication "method" — either a registered FIDO credential
/// (one wrap per authenticator) or a passphrase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrappedKey {
    /// Which derivation produced the AES key for this wrap.
    /// `#[serde(default = ...)]` keeps pre-#164-passphrase
    /// envelopes (which only had FIDO PRF wraps) deserialising
    /// cleanly as `FidoPrf`.
    #[serde(default = "default_wrap_kind")]
    pub kind: WrapKind,
    /// Base64-encoded WebAuthn credential id (FIDO PRF wraps).
    /// Empty for passphrase wraps; a synthetic id is stored
    /// instead so the entry has a stable identity for the UI's
    /// "remove this method" action.
    pub credential_id: String,
    /// Base64-encoded random 32-byte salt.  PRF input for FIDO,
    /// PBKDF2 salt for passphrase.
    pub salt: String,
    /// User-readable label.  `"YubiKey 5C"`, `"Touch ID — MacBook"`,
    /// `"Recovery passphrase"`.
    pub label: String,
    /// Base64-encoded AES-GCM nonce (12 bytes).
    pub nonce: String,
    /// Base64-encoded AES-GCM ciphertext + tag.
    pub ciphertext: String,
    /// Unix epoch seconds — purely informational, surfaced in the
    /// Settings list.
    pub created_at: i64,
}

fn default_wrap_kind() -> WrapKind {
    WrapKind::FidoPrf
}

/// The keychain entry's payload.  See the module-level doc for the
/// exact JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeychainEnvelope {
    pub version: u32,
    /// 64-character lowercase hex master key.  Present in plain
    /// mode and during the Phase 1A grace period; null once
    /// FIDO-only mode is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plain_key: Option<String>,
    #[serde(default)]
    pub wraps: Vec<WrappedKey>,
    /// When true, the unlock IPC wipes the encrypted cache after
    /// `max_unlock_attempts` consecutive failed attempts.  Off by
    /// default — opt-in destruction.
    #[serde(default)]
    pub wipe_on_failure: bool,
    /// Maximum consecutive failed unlock attempts before the
    /// cache is wiped.  `None` (or `wipe_on_failure = false`)
    /// means unlimited retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unlock_attempts: Option<u32>,
    /// Persisted across launches so kill+relaunch can't reset
    /// the wipe-on-failure budget.  Bumped before each unlock
    /// attempt in `note_unlock_failure` and cleared on success.
    #[serde(default)]
    pub failed_attempts: u32,
    /// HMAC-SHA256 (base64) over the envelope with this field
    /// blanked out, keyed by `ENVELOPE_MAC_KEY`.  Detects casual
    /// edits to `failed_attempts`, `wipe_on_failure`, and
    /// `max_unlock_attempts` via a text editor or `keyctl`-style
    /// keychain inspection tools.  Not a defence against an
    /// attacker who has the binary (the key is constant) — it
    /// raises the bar from "open in vim" to "reverse-engineer
    /// and script."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity_mac: Option<String>,
}

/// Constant HMAC key for envelope tamper detection.  Embedded in
/// the binary, so an attacker with disassembly access can forge
/// the MAC.  The point isn't cryptographic secrecy — it's that
/// editing the JSON in-place (the casual attack we're trying
/// to deter) breaks the MAC and forces the wipe to fire.
const ENVELOPE_MAC_KEY: &[u8; 32] = b"unkai-envelope-integrity-v1!!!!!";

/// Compute the integrity MAC over a serialized envelope with
/// `integrity_mac` blanked out.  Stable across runs as long as
/// the rest of the envelope content is byte-identical.
pub fn compute_envelope_mac(env: &KeychainEnvelope) -> Result<String, UnkaiError> {
    use hmac::Mac;
    let mut probe = env.clone();
    probe.integrity_mac = None;
    let bytes = serde_json::to_vec(&probe)
        .map_err(|e| UnkaiError::Storage(format!("envelope MAC serialize: {e}")))?;
    let mut mac = <Hmac<Sha256> as hmac::Mac>::new_from_slice(ENVELOPE_MAC_KEY)
        .map_err(|e| UnkaiError::Storage(format!("envelope MAC init: {e}")))?;
    mac.update(&bytes);
    Ok(B64.encode(mac.finalize().into_bytes()))
}

/// Verify the integrity MAC.  Returns `Ok(true)` when the MAC is
/// present and matches, `Ok(false)` when the field is missing
/// (legacy envelope or first save) or when it doesn't match.
pub fn verify_envelope_mac(env: &KeychainEnvelope) -> Result<bool, UnkaiError> {
    let Some(stored) = env.integrity_mac.as_deref() else {
        return Ok(false);
    };
    let expected = compute_envelope_mac(env)?;
    Ok(constant_time_eq(stored.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl KeychainEnvelope {
    pub fn new_plain(plain_key_hex: String) -> Self {
        Self {
            version: 1,
            plain_key: Some(plain_key_hex),
            wraps: Vec::new(),
            wipe_on_failure: false,
            max_unlock_attempts: None,
            failed_attempts: 0,
            integrity_mac: None,
        }
    }
}

/// Parse the keychain entry's stored value into an envelope.  Old
/// builds wrote a bare hex string; we treat that as a plain-only
/// envelope so the migration is invisible to the caller.
pub fn parse_envelope(raw: &str) -> Result<KeychainEnvelope, UnkaiError> {
    if looks_like_plain_hex(raw) {
        return Ok(KeychainEnvelope::new_plain(raw.to_string()));
    }
    serde_json::from_str(raw)
        .map_err(|e| UnkaiError::Storage(format!("master-key envelope decode: {e}")))
}

fn looks_like_plain_hex(s: &str) -> bool {
    s.len() == MASTER_KEY_LEN * 2 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Serialise an envelope for the keychain.
pub fn serialize_envelope(env: &KeychainEnvelope) -> Result<String, UnkaiError> {
    serde_json::to_string(env)
        .map_err(|e| UnkaiError::Storage(format!("master-key envelope encode: {e}")))
}

// ── Wrap helpers ──────────────────────────────────────────────

/// Seal `master_key` under the FIDO PRF output.  Used at enrollment
/// time once the frontend has run WebAuthn and ferried the PRF bytes
/// back to us.
///
/// The salt is generated here (not by the caller) so each wrap has
/// independent, fresh entropy — it's encoded into the returned
/// `WrappedKey.salt` and *must* be supplied to WebAuthn at unlock
/// time as the PRF eval input.
pub fn wrap_master_key(
    kind: WrapKind,
    master_key: &[u8],
    aes_key: &[u8],
    credential_id: &[u8],
    salt: &[u8],
    label: String,
) -> Result<WrappedKey, UnkaiError> {
    if master_key.len() != MASTER_KEY_LEN {
        return Err(UnkaiError::Storage(format!(
            "wrap_master_key: master_key must be {MASTER_KEY_LEN} bytes, got {}",
            master_key.len()
        )));
    }
    if aes_key.len() != PRF_LEN {
        return Err(UnkaiError::Storage(format!(
            "wrap_master_key: aes_key must be {PRF_LEN} bytes, got {}",
            aes_key.len()
        )));
    }
    let mut nonce_bytes = [0u8; NONCE_LEN];
    fill(&mut nonce_bytes).map_err(|e| UnkaiError::Storage(format!("wrap nonce RNG: {e}")))?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(aes_key));
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: master_key,
                aad: credential_id,
            },
        )
        .map_err(|e| UnkaiError::Storage(format!("AES-GCM seal: {e}")))?;
    Ok(WrappedKey {
        kind,
        credential_id: B64.encode(credential_id),
        salt: B64.encode(salt),
        label,
        nonce: B64.encode(nonce_bytes),
        ciphertext: B64.encode(&ct),
        created_at: Utc::now().timestamp(),
    })
}

/// Derive a 32-byte AES key from a user passphrase and the
/// stored salt via PBKDF2-HMAC-SHA-256.  Same input always
/// produces the same output, so the user can unlock by typing
/// the passphrase again.
pub fn derive_passphrase_key(passphrase: &str, salt: &[u8]) -> Result<[u8; PRF_LEN], UnkaiError> {
    if passphrase.is_empty() {
        return Err(UnkaiError::Other("passphrase must not be empty".into()));
    }
    let mut out = [0u8; PRF_LEN];
    pbkdf2::<Hmac<Sha256>>(
        passphrase.as_bytes(),
        salt,
        PASSPHRASE_PBKDF2_ITERS,
        &mut out,
    )
    .map_err(|e| UnkaiError::Storage(format!("pbkdf2 derive: {e}")))?;
    Ok(out)
}

/// Generate a synthetic credential id for a passphrase wrap.
/// Lets the UI uniquely identify and remove a passphrase entry
/// the same way it'd identify a FIDO credential.  16 random
/// bytes — collisions are not a concern.
pub fn generate_passphrase_id() -> Result<[u8; 16], UnkaiError> {
    let mut buf = [0u8; 16];
    fill(&mut buf).map_err(|e| UnkaiError::Storage(format!("synth id RNG: {e}")))?;
    Ok(buf)
}

/// Open a single wrap with the FIDO PRF output computed at unlock
/// time.  Returns the recovered master key bytes (32 bytes).
pub fn unwrap_master_key(wrap: &WrappedKey, prf_output: &[u8]) -> Result<Vec<u8>, UnkaiError> {
    if prf_output.len() != PRF_LEN {
        return Err(UnkaiError::Storage(format!(
            "unwrap_master_key: prf_output must be {PRF_LEN} bytes, got {}",
            prf_output.len()
        )));
    }
    let credential_id = B64
        .decode(wrap.credential_id.as_bytes())
        .map_err(|e| UnkaiError::Storage(format!("wrap credential_id b64: {e}")))?;
    let nonce_bytes = B64
        .decode(wrap.nonce.as_bytes())
        .map_err(|e| UnkaiError::Storage(format!("wrap nonce b64: {e}")))?;
    if nonce_bytes.len() != NONCE_LEN {
        return Err(UnkaiError::Storage(format!(
            "wrap nonce must be {NONCE_LEN} bytes, got {}",
            nonce_bytes.len()
        )));
    }
    let ct = B64
        .decode(wrap.ciphertext.as_bytes())
        .map_err(|e| UnkaiError::Storage(format!("wrap ciphertext b64: {e}")))?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(prf_output));
    let pt = cipher
        .decrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: &ct,
                aad: &credential_id,
            },
        )
        .map_err(|e| UnkaiError::Storage(format!("AES-GCM open: {e}")))?;
    if pt.len() != MASTER_KEY_LEN {
        return Err(UnkaiError::Storage(format!(
            "unwrapped master key has wrong length: {}",
            pt.len()
        )));
    }
    Ok(pt)
}

/// Generate a fresh random salt for a new wrap.  The frontend feeds
/// this back into WebAuthn's `prf.eval.first` at unlock time.
pub fn generate_salt() -> Result<[u8; SALT_LEN], UnkaiError> {
    let mut buf = [0u8; SALT_LEN];
    fill(&mut buf).map_err(|e| UnkaiError::Storage(format!("salt RNG: {e}")))?;
    Ok(buf)
}

/// Decode a base64-encoded credential id (helper for the Tauri layer).
pub fn decode_b64(s: &str) -> Result<Vec<u8>, UnkaiError> {
    B64.decode(s.as_bytes())
        .map_err(|e| UnkaiError::Storage(format!("base64 decode: {e}")))
}

/// Encode bytes as base64 (helper for the Tauri layer).
pub fn encode_b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_then_unwrap_roundtrips_fido() {
        let master = [0xAB_u8; MASTER_KEY_LEN];
        let prf = [0xCD_u8; PRF_LEN];
        let cred = b"fake-credential-id";
        let salt = generate_salt().unwrap();
        let wrap =
            wrap_master_key(WrapKind::FidoPrf, &master, &prf, cred, &salt, "Test".into()).unwrap();
        let recovered = unwrap_master_key(&wrap, &prf).unwrap();
        assert_eq!(recovered, master);
    }

    #[test]
    fn wrap_then_unwrap_roundtrips_passphrase() {
        let master = [0x77_u8; MASTER_KEY_LEN];
        let salt = generate_salt().unwrap();
        let id = generate_passphrase_id().unwrap();
        let key = derive_passphrase_key("correct horse battery staple", &salt).unwrap();
        let wrap = wrap_master_key(
            WrapKind::Passphrase,
            &master,
            &key,
            &id,
            &salt,
            "Recovery".into(),
        )
        .unwrap();
        let derived = derive_passphrase_key("correct horse battery staple", &salt).unwrap();
        let recovered = unwrap_master_key(&wrap, &derived).unwrap();
        assert_eq!(recovered, master);
    }

    #[test]
    fn unwrap_with_wrong_passphrase_fails() {
        let master = [0x01_u8; MASTER_KEY_LEN];
        let salt = generate_salt().unwrap();
        let id = generate_passphrase_id().unwrap();
        let key = derive_passphrase_key("right answer", &salt).unwrap();
        let wrap =
            wrap_master_key(WrapKind::Passphrase, &master, &key, &id, &salt, "X".into()).unwrap();
        let wrong = derive_passphrase_key("wrong answer", &salt).unwrap();
        assert!(unwrap_master_key(&wrap, &wrong).is_err());
    }

    #[test]
    fn unwrap_with_wrong_prf_fails() {
        let master = [0x01_u8; MASTER_KEY_LEN];
        let prf = [0x02_u8; PRF_LEN];
        let wrong_prf = [0x03_u8; PRF_LEN];
        let cred = b"id";
        let salt = generate_salt().unwrap();
        let wrap =
            wrap_master_key(WrapKind::FidoPrf, &master, &prf, cred, &salt, "X".into()).unwrap();
        assert!(unwrap_master_key(&wrap, &wrong_prf).is_err());
    }

    #[test]
    fn plain_hex_treated_as_plain_envelope() {
        let hex = "a".repeat(64);
        let env = parse_envelope(&hex).unwrap();
        assert_eq!(env.plain_key.as_deref(), Some(hex.as_str()));
        assert!(env.wraps.is_empty());
    }
}

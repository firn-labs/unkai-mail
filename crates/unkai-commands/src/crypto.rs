//! OpenPGP and S/MIME key management plus on-demand decrypt.
//!
//! Mirrors `ui/src/lib/api/crypto.ts`.

use unkai_core::UnkaiError;
use unkai_core::models::Email;
use unkai_store::Cache;
use unkai_store::account_store;
use unkai_store::cache::PgpKeySource;
use unkai_store::cache::PgpPublicKeyRow;
use unkai_store::cache::SmimeCertRow;
use unkai_store::cache::SmimeCertSource;
use unkai_store::credentials;
use zeroize::Zeroizing;

use crate::crypto_bridge::AppCryptoBridge;
use crate::state::SettingsSyncNotify;
use crate::support::{connect_imap, connect_jmap, load_account, uses_jmap};

/// Decrypt an encrypted message on demand (#57).
///
/// Called by MailView when the user clicks "Decrypt" on a message
/// the receive path marked `protection = "encrypted"`.  Composes a
/// `AppCryptoBridge` from the freshly-prompted (or keychain-
/// resolved) passphrase and runs the raw `.eml` bytes through
/// `parse_eml_bytes_with_crypto` so decryption + re-parse happen
/// in one place.
///
/// **Bytes source order (#341 ciphertext cache):**
///   1. `Cache::get_encrypted_raw_eml` — populated by any previous
///      decrypt / attachment fetch / background-decrypt for this
///      UID.  Hit = full decrypt without an IMAP / JMAP round-trip
///      (works offline).
///   2. Cache miss → fetch from the server: IMAP `UID FETCH
///      BODY.PEEK[]` on IMAP accounts, JMAP `Blob/get` via the
///      session's download URL (#341) on JMAP accounts.  On
///      success stash the bytes for next time.
///
/// Flags (Seen / Flagged) come from a parallel envelope fetch on
/// the server path; on the cache-hit path we pull them from the
/// envelope row already in the cache (which the user just saw in
/// MailView, so it's at least as fresh as the displayed list).
pub async fn decrypt_message(
    account_id: String,
    folder: String,
    uid: u32,
    pgp_passphrase: String,
    cache: &Cache,
) -> Result<Email, UnkaiError> {
    let account = load_account(cache, &account_id)?;

    // #338 — the message could be PGP or S/MIME and we don't know which
    // until we parse the envelope, so build a receive bridge that loads
    // whichever stacks the account has.  The typed passphrase (empty →
    // keychain "Unlock automatically" entry, per stack) is tried against
    // both; if the stack the message actually needs couldn't be unlocked
    // the per-stack decrypt surfaces a clean `Auth` error below, routing
    // the user back to the Decrypt input or the Encryption Settings
    // opt-in just as the pre-resolve check used to.
    let bridge =
        AppCryptoBridge::for_account_receive(&account_id, &pgp_passphrase, (*cache).clone());

    let id = format!("{folder}:{uid}");

    // #341 ciphertext cache — try the local copy first.  A
    // successful path returns without ever opening an IMAP
    // connection, so this is also the path the offline UX walks.
    // Any failure (corrupt cache row, key rotated, etc.) falls
    // through to the IMAP refetch below rather than surfacing as a
    // permanent decrypt error — a stale cache entry mustn't brick
    // the user's ability to read their mail.
    if let Ok(Some(raw)) = cache.get_encrypted_raw_eml(&account_id, &folder, uid) {
        match unkai_imap::parse_eml_bytes_with_crypto(
            &raw,
            &id,
            &account_id,
            &folder,
            Some(&bridge),
        ) {
            Ok(mut decrypted) => {
                // Pull is_read / is_starred from the cached
                // envelope so we don't reset them via
                // `parse_eml_bytes_with_crypto`'s defaults.  The
                // envelope cache is refreshed by the next poll
                // tick — for the user clicking Decrypt right now
                // it's as fresh as the MailList row they just
                // clicked from.
                if let Ok(Some(env)) = cache.get_message(&account_id, &folder, uid) {
                    decrypted.is_read = env.is_read;
                    decrypted.is_starred = env.is_starred;
                    // #414: local-only state, same overlay reason.
                    decrypted.is_pinned = env.is_pinned;
                    decrypted.priority_override = env.priority_override;
                    // #416: same overlay + self-request suppression
                    // as the network path below.
                    decrypted.mdn_handled = env.mdn_handled;
                }
                if decrypted
                    .mdn_requested_to
                    .as_deref()
                    .is_some_and(|dnt| addresses_match(dnt, &account.email))
                {
                    decrypted.mdn_requested_to = None;
                }
                if let Err(e) = cache.upsert_message(&decrypted) {
                    tracing::warn!("cache.upsert_message after offline decrypt failed: {e}");
                }
                return Ok(decrypted);
            }
            Err(e) => {
                tracing::warn!(
                    "decrypt_message: cached ciphertext for \
                     {account_id}/{folder}/{uid} failed to decrypt ({e}); \
                     refetching from IMAP"
                );
            }
        }
    }

    // Get envelope (Seen / Flagged) + raw bytes from the server.
    // IMAP reuses one session for both calls; JMAP issues two HTTP
    // round-trips (Email/get for blobId + the download URL) but is
    // stateless so no logout is needed.  We need the envelope flags
    // so the post-decrypt cache write below doesn't reset
    // is_read / is_starred via the bridge-aware parser's defaults.
    let (envelope_email, raw) = if uses_jmap(&account) {
        let client = connect_jmap(&account).await?;
        let env = client.fetch_message(&folder, uid, &account_id).await?;
        let raw = client.fetch_raw_message(&folder, uid).await?;
        (env, raw)
    } else {
        let mut client = connect_imap(&account).await?;
        let env = client.fetch_message(&folder, uid, &account_id).await?;
        let raw = client.fetch_raw_message(&folder, uid).await?;
        let _ = client.logout().await;
        (env, raw)
    };

    let mut decrypted =
        unkai_imap::parse_eml_bytes_with_crypto(&raw, &id, &account_id, &folder, Some(&bridge))?;
    // Overlay server-side flags so the cache write below doesn't
    // reset them — `parse_eml_bytes_with_crypto` defaults to
    // is_read=true when it has no IMAP / JMAP context.
    decrypted.is_read = envelope_email.is_read;
    decrypted.is_starred = envelope_email.is_starred;
    // #414/#415/#416: pin + priority-override + reminder +
    // receipt-handled live only in the cache; the server-side
    // envelope fetch above can't carry them.
    if let Ok(Some((is_pinned, priority_override, reminder_at, mdn_handled))) =
        cache.envelope_local_state(&account_id, &folder, uid)
    {
        decrypted.is_pinned = is_pinned;
        decrypted.priority_override = priority_override;
        decrypted.reminder_at = reminder_at;
        decrypted.mdn_handled = mdn_handled;
    }

    // #416: same self-request suppression as `fetch_message_inner` —
    // our own sent copies carry our own address in
    // `Disposition-Notification-To`, and a receipt to ourselves is
    // meaningless.
    if decrypted
        .mdn_requested_to
        .as_deref()
        .is_some_and(|dnt| addresses_match(dnt, &account.email))
    {
        decrypted.mdn_requested_to = None;
    }

    if let Err(e) = cache.upsert_message(&decrypted) {
        tracing::warn!("cache.upsert_message after decrypt failed: {e}");
    }
    // Only cache the raw bytes when the parser actually unlocked a
    // PGP/MIME envelope — caching plaintext bytes would just bloat
    // the DB without ever paying off, since the cache-hit path is
    // only exercised by `decrypt_message` / encrypted-attachment
    // downloads.  The parser stamps `protection` to one of the
    // encryption labels exactly when a PGP/MIME envelope was
    // detected and processed.
    if matches!(
        decrypted.protection.as_deref(),
        Some("encrypted" | "signed-and-encrypted")
    ) && let Err(e) = cache.put_encrypted_raw_eml(&account_id, &folder, uid, &raw)
    {
        tracing::warn!("cache.put_encrypted_raw_eml after decrypt failed: {e}");
    }
    Ok(decrypted)
}

/// Silent variant of [`decrypt_message`] for the auto-decrypt path
/// (#341).
///
/// `MailView.load()` calls this the moment an encrypted message
/// becomes visible — instead of waiting for the user to click
/// **Decrypt** and type a passphrase.  Returns:
///   - `Ok(None)` when the account hasn't opted into
///     "Unlock automatically" (no keychain entry).  Renderer falls
///     back to showing the manual Decrypt button as before.
///   - `Ok(Some(email))` when the keychain held a passphrase AND
///     it unlocked the message.  Body is overlaid with plaintext
///     and the cache row is updated transactionally.
///   - `Err` when the keychain entry exists but failed to decrypt
///     (passphrase no longer matches, key was rotated, ciphertext
///     corrupt, …).  Renderer surfaces the error and offers the
///     manual prompt so the user can recover.
///
/// Separating success-without-attempt from outright failure keeps
/// the renderer's UX honest: a no-opt-in account never sees an
/// error message about a feature it didn't enable.
pub async fn try_auto_decrypt_message(
    account_id: String,
    folder: String,
    uid: u32,
    cache: &Cache,
) -> Result<Option<Email>, UnkaiError> {
    if !credentials::has_pgp_passphrase(&account_id)? {
        return Ok(None);
    }
    let email = decrypt_message(
        account_id,
        folder,
        uid,
        // Empty passphrase routes through `resolve_pgp_passphrase`
        // → keychain — exactly the path the user opted into.
        String::new(),
        cache,
    )
    .await?;
    Ok(Some(email))
}

/// Counterpart to `download_email_attachment` for PGP/MIME messages
/// (#341 follow-up to #57).  The plain command walks the *outer*
/// `multipart/encrypted` envelope with whatever `part_id` it gets,
/// which for a decrypted message would return the `Version: 1`
/// header part instead of the real attachment bytes — `EmailAttachment.part_id`
/// on a decrypted message indexes the *inner* tree.  This command
/// pulls the raw IMAP / JMAP bytes, decrypts through the bridge
/// built from the account's stored key + the freshly-prompted
/// passphrase, walks the inner tree with the same primary-then-
/// fallback `attachments()` / `parts` lookup the plaintext path
/// uses, and returns those bytes.
///
/// IMAP and JMAP both go through the same
/// `extract_decrypted_attachment` helper; the only difference is
/// where the raw `.eml` comes from — IMAP `UID FETCH BODY.PEEK[]`
/// vs. JMAP `Blob/get` via the session's download URL (#341).
pub async fn download_decrypted_attachment(
    account_id: String,
    folder: String,
    uid: u32,
    part_id: u32,
    pgp_passphrase: String,
    cache: &Cache,
) -> Result<Vec<u8>, UnkaiError> {
    let account = load_account(cache, &account_id)?;
    // #338 — same dual-stack receive bridge as `decrypt_message`: the
    // attachment may live inside a PGP/MIME or an S/MIME enveloped
    // message, and `extract_decrypted_attachment` picks the right stack.
    // Empty passphrase falls back to each stack's keychain
    // "Unlock automatically" entry.
    let bridge =
        AppCryptoBridge::for_account_receive(&account_id, &pgp_passphrase, (*cache).clone());

    // #341 ciphertext cache — try the local copy first so a second
    // attachment open / Forward of the same encrypted message
    // doesn't pay the server cost again.  On cache miss (or a
    // decrypt-from-cache failure) fall through to the network and
    // populate the cache from the fresh bytes.
    if let Ok(Some(raw)) = cache.get_encrypted_raw_eml(&account_id, &folder, uid) {
        match unkai_imap::extract_decrypted_attachment(&raw, &bridge, part_id) {
            Ok(Some((_meta, data))) => return Ok(data),
            // Cached bytes aren't a PGP/MIME envelope after all —
            // same typed error as the live-fetch path so the UI's
            // encryption-aware routing stays consistent.
            Ok(None) => {
                return Err(UnkaiError::Protocol(
                    "Message is not encrypted; use download_email_attachment".into(),
                ));
            }
            Err(e) => {
                tracing::warn!(
                    "download_decrypted_attachment: cached ciphertext for \
                     {account_id}/{folder}/{uid} failed ({e}); refetching from server"
                );
            }
        }
    }

    let raw = if uses_jmap(&account) {
        let client = connect_jmap(&account).await?;
        client.fetch_raw_message(&folder, uid).await?
    } else {
        let mut client = connect_imap(&account).await?;
        let raw = client.fetch_raw_message(&folder, uid).await?;
        let _ = client.logout().await;
        raw
    };
    match unkai_imap::extract_decrypted_attachment(&raw, &bridge, part_id)? {
        Some((_meta, data)) => {
            // Best-effort cache write — a failure here just means
            // the next attachment download pays the network cost
            // again.  Logged at debug because the user's request
            // succeeded.
            if let Err(e) = cache.put_encrypted_raw_eml(&account_id, &folder, uid, &raw) {
                tracing::debug!("put_encrypted_raw_eml after attachment fetch failed: {e}");
            }
            Ok(data)
        }
        // Not a PGP/MIME envelope — the caller should be on
        // `download_email_attachment` for this message.  Surfacing
        // as a typed error rather than silently falling through
        // keeps the UI's encryption-aware routing honest.
        None => Err(UnkaiError::Protocol(
            "Message is not encrypted; use download_email_attachment".into(),
        )),
    }
}

/// Reduce a `Name <addr@host>` mailbox (or a bare address) to the
/// lowercased `addr@host` alone (#416).  Used to compare header
/// addresses against the account's own address without caring about
/// display names, quoting, or case.
pub fn bare_address(value: &str) -> String {
    let v = value.trim();
    match (v.rfind('<'), v.rfind('>')) {
        (Some(start), Some(end)) if start < end => v[start + 1..end].trim().to_lowercase(),
        _ => v.trim_matches('"').trim().to_lowercase(),
    }
}

/// Do two mailbox strings name the same address (#416)?  Empty
/// values never match — a malformed header shouldn't accidentally
/// equal a malformed account entry.
pub fn addresses_match(a: &str, b: &str) -> bool {
    let (a, b) = (bare_address(a), bare_address(b));
    !a.is_empty() && a == b
}

// ── End-to-end encryption (#57) ──────────────────────────────────
//
// Tauri commands + the concrete `CryptoBridge` implementation that
// the IMAP receive and SMTP send paths consume.  All the protocol
// plumbing is in the `unkai-crypto` + `unkai-imap` + `unkai-smtp`
// crates; this module just stitches them together with the cache
// (recipient public-key lookup) and the OS keychain (private-key
// material + passphrase) when an IPC fires.

/// What the AccountSettings panel displays for an account's PGP
/// state.  `has_key` is the cheap signal ("show import button vs.
/// show fingerprint + remove button"); `fingerprint` is the human-
/// readable identifier when present.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PgpKeyStatus {
    pub has_key: bool,
    pub fingerprint: Option<String>,
}

/// Import + persist an OpenPGP private key for an account.
///
/// The `passphrase` argument is used to validate the key parses
/// (proving the user typed the right one before we accept the
/// import); after that it's dropped.  Per the "re-prompt per
/// operation" decision in #57 the passphrase is **not** stashed in
/// the keychain — the UI prompts for it again every time encryption
/// or decryption fires.
///
/// Side effects: armored key written to the OS keychain, the
/// fingerprint cached on the `accounts` row so the UI can render
/// "Key 9F2A…AAAA" without unlocking the keychain.
pub async fn pgp_import_private_key(
    account_id: String,
    armored_key: String,
    passphrase: String,
    cache: &Cache,
    notify: &SettingsSyncNotify,
) -> Result<String, UnkaiError> {
    // Wrap the cleartext secrets so their heap buffers are scrubbed on
    // drop rather than lingering in freed memory (#370).  These are the
    // longest-lived copies in our control — Tauri allocated them when it
    // deserialised the IPC payload.
    let armored_key = Zeroizing::new(armored_key);
    let passphrase = Zeroizing::new(passphrase);

    let parsed = unkai_crypto::parse_private_key(armored_key.as_bytes(), Some(passphrase.as_str()))
        .map_err(|e| UnkaiError::Crypto(format!("PGP key import failed: {e}")))?;
    let fingerprint = parsed.fingerprint();
    // Drop the parsed key + passphrase immediately — we just used
    // them to verify the import.  The next encrypt / decrypt will
    // re-parse against a fresh passphrase the user types.
    drop(parsed);

    credentials::store_pgp_private_key(&account_id, armored_key.as_str())?;

    // Update the account row so the AccountSettings UI sees the
    // fingerprint on its next reload without having to crack open
    // the keychain.  Loading + saving preserves every other field —
    // the IPC contract from #115 already takes a full Account on
    // update_account, so we follow suit.
    let accounts = account_store::load_accounts(cache)?;
    if let Some(mut acc) = accounts.into_iter().find(|a| a.id == account_id) {
        acc.pgp_key_fingerprint = Some(fingerprint.clone());
        account_store::update_account(cache, acc)?;
        notify.0.notify_one();
    }

    Ok(fingerprint)
}

/// Remove the OpenPGP private key for an account.  Mirrors the IMAP
/// password removal path: clears the keychain entry(s) and drops the
/// fingerprint hint from the account row.  Also clears any orphaned
/// passphrase entry from an older build that pre-dated the
/// "re-prompt per operation" decision — defensive cleanup.
pub fn pgp_remove_private_key(
    account_id: String,
    cache: &Cache,
    notify: &SettingsSyncNotify,
) -> Result<(), UnkaiError> {
    credentials::delete_pgp_private_key(&account_id)?;
    credentials::delete_pgp_passphrase(&account_id)?;

    let accounts = account_store::load_accounts(cache)?;
    if let Some(mut acc) = accounts.into_iter().find(|a| a.id == account_id)
        && acc.pgp_key_fingerprint.is_some()
    {
        acc.pgp_key_fingerprint = None;
        account_store::update_account(cache, acc)?;
        notify.0.notify_one();
    }
    Ok(())
}

/// Enable "Unlock automatically" for an account (#341).
///
/// Validates that the supplied passphrase actually unlocks the
/// account's stored PGP private key, then writes the passphrase to
/// the OS keychain under the `unkai-mail-pgp-passphrase` service
/// slot already provisioned at #57.  From the moment this returns
/// `Ok(())` the rest of the encrypt / decrypt machinery picks up
/// the stored passphrase whenever the frontend hands over `null`
/// (or an empty string) for `pgp_passphrase`, and the IMAP receive
/// path background-decrypts new mail without the user having to
/// click anything.
///
/// We validate by re-parsing the armored key against the supplied
/// passphrase — same idiom as `pgp_import_private_key` — so a typo
/// fails fast with a typed `Crypto` error instead of being saved
/// and silently breaking every subsequent operation on the account.
pub fn pgp_enable_unlock_automatically(
    account_id: String,
    passphrase: String,
) -> Result<(), UnkaiError> {
    // Both the passphrase the user typed and the armored private key we
    // pull back from the keychain are cleartext secrets — scrub them on
    // drop (#370).
    let passphrase = Zeroizing::new(passphrase);
    let armored = Zeroizing::new(credentials::get_pgp_private_key(&account_id)?);
    let parsed = unkai_crypto::parse_private_key(armored.as_bytes(), Some(passphrase.as_str()))
        .map_err(|e| UnkaiError::Crypto(format!("PGP key parse failed: {e}")))?;
    // `parse_private_key` deliberately does NOT check the
    // passphrase — rpgp defers secret-packet decryption until the
    // key material is actually needed, so a wrong passphrase
    // sails through the parse step with no error.  Actually
    // exercise the passphrase by signing a tiny well-known payload
    // (one BLAKE3/SHA-256 hash + an rpgp signing call) — wrong
    // passphrase fails fast with a Crypto error, right passphrase
    // succeeds in single-digit milliseconds.  The signature bytes
    // are thrown away; we only care about the unlock side effect.
    unkai_crypto::sign_detached(b"unkai-passphrase-validation", &parsed)
        .map_err(|e| UnkaiError::Crypto(format!("Wrong encryption passphrase: {e}")))?;
    credentials::store_pgp_passphrase(&account_id, passphrase.as_str())?;
    Ok(())
}

/// Disable "Unlock automatically" — drops the keychain entry so
/// future encrypt / decrypt operations on this account re-prompt
/// the user the way they did before opt-in.  Idempotent: missing
/// entry is treated as already-disabled (the underlying helper
/// swallows `NoEntry`).
pub fn pgp_disable_unlock_automatically(account_id: String) -> Result<(), UnkaiError> {
    credentials::delete_pgp_passphrase(&account_id)
}

/// `true` when the account has a stored passphrase (opt-in is on),
/// `false` otherwise.  Drives the toggle state in EncryptionSettings
/// without forcing the renderer to interpret a missing-entry
/// `Auth` error from `get_pgp_passphrase` as a falsy outcome.
pub fn pgp_has_unlock_automatically(account_id: String) -> Result<bool, UnkaiError> {
    credentials::has_pgp_passphrase(&account_id)
}

/// What does the user's account look like, key-wise?  Cheap read from
/// the SQLCipher row — doesn't touch the keychain.
pub fn pgp_get_account_key_status(
    account_id: String,
    cache: &Cache,
) -> Result<PgpKeyStatus, UnkaiError> {
    let fingerprint = account_store::load_accounts(cache)?
        .into_iter()
        .find(|a| a.id == account_id)
        .and_then(|a| a.pgp_key_fingerprint);
    Ok(PgpKeyStatus {
        has_key: fingerprint.is_some(),
        fingerprint,
    })
}

/// Import a recipient's PGP public key by paste.  The
/// `email_hint` is what the user typed in the Compose key picker
/// (or the contact card they were viewing); we trust it for the
/// `email` column but the fingerprint comes from the key itself.
pub fn pgp_import_public_key(
    armored_key: String,
    email_hint: Option<String>,
    cache: &Cache,
) -> Result<String, UnkaiError> {
    let parsed = unkai_crypto::parse_public_key(armored_key.as_bytes())
        .map_err(|e| UnkaiError::Crypto(format!("Public key parse failed: {e}")))?;
    let fingerprint = parsed.fingerprint();
    let row = PgpPublicKeyRow {
        fingerprint: fingerprint.clone(),
        email: email_hint,
        armored_key,
        source: PgpKeySource::Manual,
        added_at: chrono::Utc::now().timestamp(),
    };
    cache.upsert_pgp_public_key(&row)?;
    Ok(fingerprint)
}

/// Remove one cached public key by fingerprint.
pub fn pgp_remove_public_key(fingerprint: String, cache: &Cache) -> Result<(), UnkaiError> {
    cache
        .delete_pgp_public_key(&fingerprint)
        .map_err(UnkaiError::from)
}

/// List every cached public key, newest first, for the
/// AccountSettings "Known recipient keys" panel.
pub fn pgp_list_public_keys(cache: &Cache) -> Result<Vec<PgpPublicKeyDto>, UnkaiError> {
    let rows = cache.list_pgp_public_keys().map_err(UnkaiError::from)?;
    Ok(rows.into_iter().map(PgpPublicKeyDto::from).collect())
}

/// Look up every cached public key claiming a given email address —
/// powers the per-recipient "🔑 has key" / "⚠ no key" indicator chips
/// in Compose.
pub fn pgp_get_keys_for_email(
    email: String,
    cache: &Cache,
) -> Result<Vec<PgpPublicKeyDto>, UnkaiError> {
    let rows = cache
        .get_pgp_public_keys_for_email(&email)
        .map_err(UnkaiError::from)?;
    Ok(rows.into_iter().map(PgpPublicKeyDto::from).collect())
}

/// IPC-shaped projection of `PgpPublicKeyRow` — same fields, the
/// `source` enum flattened to a string, and the armored bytes
/// omitted (the UI never needs the raw key; it only renders the
/// fingerprint + email + provenance).  Dropping the armor keeps
/// the IPC payload small even with hundreds of cached keys.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PgpPublicKeyDto {
    pub fingerprint: String,
    pub email: Option<String>,
    pub source: String,
    pub added_at: i64,
}

impl From<PgpPublicKeyRow> for PgpPublicKeyDto {
    fn from(r: PgpPublicKeyRow) -> Self {
        Self {
            fingerprint: r.fingerprint,
            email: r.email,
            source: r.source.as_str().to_string(),
            added_at: r.added_at,
        }
    }
}

// ── S/MIME (X.509) certificate management (#338) ───────────────
//
// X.509 counterpart to the OpenPGP key-management commands above.
// Same split: the user's own identity (a passphrase-protected `.p12`
// in the OS keychain, with the fingerprint cached on the account row)
// vs. cached recipient certificates (the `smime_certs` table).  The
// IPC shapes deliberately mirror the PGP DTOs so the settings UI can
// drive both stacks through parallel calls.

/// What the S/MIME settings panel displays for an account's identity
/// state.  `has_cert` is the cheap signal (import button vs.
/// fingerprint + remove button); `fingerprint` is the human-readable
/// identifier when present.  Mirrors `PgpKeyStatus`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SmimeCertStatus {
    pub has_cert: bool,
    pub fingerprint: Option<String>,
}

/// IPC-shaped projection of `SmimeCertRow`.  As with `PgpPublicKeyDto`
/// the DER blob itself is omitted (the UI only renders identifiers),
/// but we add the subject / issuer distinguished names: for X.509 the
/// subject DN is the human identity (more telling than the email), and
/// the issuer DN is what the later trust-model chunk will surface.
/// Both are derived from the stored DER through the `unkai-crypto`
/// façade so the cache schema stays minimal and DN formatting has a
/// single source of truth.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SmimeCertDto {
    pub fingerprint: String,
    pub email: Option<String>,
    pub subject_dn: String,
    pub issuer_dn: String,
    pub source: String,
    pub added_at: i64,
}

/// Build the IPC DTO for one cached cert, deriving the subject / issuer
/// DNs by re-parsing the stored DER.  A stored cert should always
/// re-parse (we only ever persist bytes that parsed on the way in), but
/// if OpenSSL ever refuses we fall back to `"unknown"` DNs rather than
/// dropping the row — the fingerprint + email are still useful and the
/// user can remove a cert the app can no longer read.
pub fn smime_cert_dto(row: SmimeCertRow) -> SmimeCertDto {
    let (subject_dn, issuer_dn) = match unkai_crypto::parse_der_cert(&row.der_cert) {
        Ok(cert) => (cert.subject_dn(), cert.issuer_dn()),
        Err(_) => ("unknown".to_string(), "unknown".to_string()),
    };
    SmimeCertDto {
        fingerprint: row.fingerprint,
        email: row.email,
        subject_dn,
        issuer_dn,
        source: row.source.as_str().to_string(),
        added_at: row.added_at,
    }
}

/// Parse a recipient certificate from whatever the UI hands us: a
/// pasted PEM block, base64-encoded DER (the `.cer` file-picker path),
/// or base64-encoded PEM.  Tries the cheapest interpretation first so
/// the common paste case never pays for a base64 decode.
pub fn parse_smime_cert_flexible(input: &str) -> Result<unkai_crypto::Certificate, UnkaiError> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    // 1. Raw PEM paste — the common case.  PEM's `-----` delimiters
    //    aren't valid base64, so this can't false-match a base64 blob.
    if let Ok(cert) = unkai_crypto::parse_pem_cert(input.as_bytes()) {
        return Ok(cert);
    }
    // 2. base64 of either DER or PEM (a file picker reads raw bytes and
    //    base64-encodes them for the IPC string boundary).
    if let Ok(decoded) = STANDARD.decode(input.trim().as_bytes()) {
        if let Ok(cert) = unkai_crypto::parse_der_cert(&decoded) {
            return Ok(cert);
        }
        if let Ok(cert) = unkai_crypto::parse_pem_cert(&decoded) {
            return Ok(cert);
        }
    }
    Err(UnkaiError::Crypto(
        "Could not parse certificate — expected X.509 PEM or DER".into(),
    ))
}

/// Import + persist the user's own S/MIME identity for an account from
/// a PKCS#12 (`.p12`) upload.
///
/// `pkcs12_base64` is the binary `.p12` base64-encoded for the IPC
/// string boundary.  The `passphrase` validates the bundle parses
/// (proving the user typed the right one before we accept the import);
/// after that it's dropped.  Per the "re-prompt per operation" decision
/// carried over from #57, the passphrase is **not** stashed in the
/// keychain here — the UI prompts for it again every time signing or
/// decryption fires, unless the user opts into "Unlock automatically".
///
/// Side effects: the raw `.p12` is written to the OS keychain, and the
/// fingerprint is cached on the `accounts` row so the settings UI can
/// render "Certificate AB:CD:…" without unlocking the keychain.
pub fn smime_import_pkcs12(
    account_id: String,
    pkcs12_base64: String,
    passphrase: String,
    cache: &Cache,
    notify: &SettingsSyncNotify,
) -> Result<String, UnkaiError> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    // Scrub the cleartext `.p12` passphrase on drop (#370).
    let passphrase = Zeroizing::new(passphrase);
    let p12_bytes = STANDARD
        .decode(pkcs12_base64.trim().as_bytes())
        .map_err(|e| UnkaiError::Crypto(format!("Invalid PKCS#12 upload encoding: {e}")))?;
    // `parse_pkcs12` both verifies the passphrase (PKCS#12 MAC check)
    // and proves the bundle carries a leaf cert + private key before we
    // store it — a wrong passphrase fails fast here with the
    // "Wrong PKCS#12 passphrase" sentinel rather than being saved and
    // breaking every later operation on the account.
    let parsed = unkai_crypto::parse_pkcs12(&p12_bytes, passphrase.as_str())?;
    let fingerprint = parsed.fingerprint();
    drop(parsed);

    credentials::store_smime_private_cert(&account_id, &p12_bytes)?;

    // Cache the fingerprint on the account row so the status read stays
    // cheap.  Load + save preserves every other field, matching the
    // PGP import path.
    let accounts = account_store::load_accounts(cache)?;
    if let Some(mut acc) = accounts.into_iter().find(|a| a.id == account_id) {
        acc.smime_cert_fingerprint = Some(fingerprint.clone());
        account_store::update_account(cache, acc)?;
        notify.0.notify_one();
    }

    Ok(fingerprint)
}

/// Remove the S/MIME identity for an account.  Mirrors the PGP
/// private-key removal: clears the keychain entries (bundle +
/// passphrase) and drops the fingerprint hint from the account row.
pub fn smime_remove_private_cert(
    account_id: String,
    cache: &Cache,
    notify: &SettingsSyncNotify,
) -> Result<(), UnkaiError> {
    credentials::delete_smime_private_cert(&account_id)?;
    credentials::delete_smime_passphrase(&account_id)?;

    let accounts = account_store::load_accounts(cache)?;
    if let Some(mut acc) = accounts.into_iter().find(|a| a.id == account_id)
        && acc.smime_cert_fingerprint.is_some()
    {
        acc.smime_cert_fingerprint = None;
        account_store::update_account(cache, acc)?;
        notify.0.notify_one();
    }
    Ok(())
}

/// Enable "Unlock automatically" for an account's S/MIME identity.
///
/// Validates that the supplied passphrase actually unlocks the stored
/// `.p12`, then writes it to the OS keychain under the
/// `unkai-mail-smime-passphrase` slot.  Unlike the OpenPGP path —
/// where parsing defers passphrase checking and we have to test-sign a
/// payload — PKCS#12 verifies the passphrase at parse time via its MAC,
/// so re-parsing the stored bundle is enough to prove the passphrase.
pub fn smime_enable_unlock_automatically(
    account_id: String,
    passphrase: String,
) -> Result<(), UnkaiError> {
    // Scrub the cleartext passphrase on drop (#370).
    let passphrase = Zeroizing::new(passphrase);
    let p12_bytes = credentials::get_smime_private_cert(&account_id)?;
    let parsed = unkai_crypto::parse_pkcs12(&p12_bytes, passphrase.as_str())?;
    drop(parsed);
    credentials::store_smime_passphrase(&account_id, passphrase.as_str())?;
    Ok(())
}

/// Disable "Unlock automatically" — drops the keychain passphrase entry
/// so future operations re-prompt.  Idempotent (missing entry is
/// treated as already-disabled).
pub fn smime_disable_unlock_automatically(account_id: String) -> Result<(), UnkaiError> {
    credentials::delete_smime_passphrase(&account_id)
}

/// `true` when the account has a stored S/MIME passphrase (opt-in is
/// on).  Drives the toggle state without forcing the renderer to read a
/// missing-entry `Auth` error as falsy.
pub fn smime_has_unlock_automatically(account_id: String) -> Result<bool, UnkaiError> {
    credentials::has_smime_passphrase(&account_id)
}

/// What does the account look like, S/MIME-identity-wise?  Cheap read
/// from the SQLCipher row — doesn't touch the keychain.
pub fn smime_get_account_cert_status(
    account_id: String,
    cache: &Cache,
) -> Result<SmimeCertStatus, UnkaiError> {
    let fingerprint = account_store::load_accounts(cache)?
        .into_iter()
        .find(|a| a.id == account_id)
        .and_then(|a| a.smime_cert_fingerprint);
    Ok(SmimeCertStatus {
        has_cert: fingerprint.is_some(),
        fingerprint,
    })
}

/// Import a recipient's S/MIME certificate by paste or file upload.
/// The `email_hint` is what the user typed (or the contact card they
/// were viewing); we prefer it for the `email` column but fall back to
/// the cert's own SAN rfc822Name.  The fingerprint always comes from
/// the certificate itself.
pub fn smime_import_public_cert(
    cert_data: String,
    email_hint: Option<String>,
    cache: &Cache,
) -> Result<String, UnkaiError> {
    let cert = parse_smime_cert_flexible(&cert_data)?;
    let fingerprint = cert.fingerprint();
    let row = SmimeCertRow {
        fingerprint: fingerprint.clone(),
        email: email_hint.or_else(|| cert.email()),
        der_cert: cert.to_der()?,
        source: SmimeCertSource::Manual,
        added_at: chrono::Utc::now().timestamp(),
    };
    cache.upsert_smime_cert(&row)?;
    Ok(fingerprint)
}

/// Remove one cached certificate by fingerprint.
pub fn smime_remove_public_cert(fingerprint: String, cache: &Cache) -> Result<(), UnkaiError> {
    cache
        .delete_smime_cert(&fingerprint)
        .map_err(UnkaiError::from)
}

/// List every cached certificate, newest first, for the S/MIME
/// settings "Known recipient certificates" panel.
pub fn smime_list_public_certs(cache: &Cache) -> Result<Vec<SmimeCertDto>, UnkaiError> {
    let rows = cache.list_smime_certs().map_err(UnkaiError::from)?;
    Ok(rows.into_iter().map(smime_cert_dto).collect())
}

/// Look up every cached certificate claiming a given email address —
/// powers the per-recipient "has cert" indicator chips in Compose.
pub fn smime_get_certs_for_email(
    email: String,
    cache: &Cache,
) -> Result<Vec<SmimeCertDto>, UnkaiError> {
    let rows = cache
        .get_smime_certs_for_email(&email)
        .map_err(UnkaiError::from)?;
    Ok(rows.into_iter().map(smime_cert_dto).collect())
}

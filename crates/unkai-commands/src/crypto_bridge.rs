//! The `CryptoBridge` implementation wired into the protocol crates
//! (#57 / #338, extracted in #476).
//!
//! `unkai-core` declares the trait so `unkai-imap` / `unkai-smtp` can
//! ask "decrypt this" or "sign that" without depending on a crypto
//! stack.  This is the concrete implementation that reaches into
//! `unkai-crypto` and the keychain.
//!
//! It was called `AppCryptoBridge` when it lived in `main.rs`.  It
//! never touched a Tauri type — the name only ever meant "the bridge
//! the Tauri layer installs" — so moving it out of that layer renames
//! it to `AppCryptoBridge`.

use unkai_core::UnkaiError;
use unkai_store::Cache;
use unkai_store::cache::PgpKeySource;
use unkai_store::cache::PgpPublicKeyRow;
use unkai_store::cache::SmimeCertRow;
use unkai_store::cache::SmimeCertSource;
use unkai_store::credentials;

use crate::contacts::{decode_vcard_key_value, decode_vcard_smime_cert_value};

/// Pick the passphrase that should unlock this account's PGP key
/// for one operation (#341).
///
/// The rule:
///   - Non-empty caller-supplied value wins.  This is the case
///     where the user typed a passphrase into the MailView Decrypt
///     input or Compose's encryption ribbon — they explicitly
///     overrode whatever the keychain holds, so we trust the
///     freshly-typed value.
///   - Empty / missing caller value falls back to the keychain
///     entry written when the user enabled "Unlock automatically"
///     in Encryption Settings.  No keychain entry means no opt-in
///     for this account, which surfaces as a clean `Auth` error
///     the IPC layer can map to the right re-prompt UI.
///
/// Centralising the rule here (rather than copy-pasting the
/// `if empty { keychain } else { typed }` pattern across every
/// passphrase-consuming command) keeps the precedence consistent —
/// any future operation that takes a passphrase should call this
/// helper instead of inventing its own resolution.
pub fn resolve_pgp_passphrase(account_id: &str, supplied: &str) -> Result<String, UnkaiError> {
    if !supplied.is_empty() {
        return Ok(supplied.to_string());
    }
    credentials::get_pgp_passphrase(account_id)
}

/// S/MIME counterpart to [`resolve_pgp_passphrase`] (#338).  Same
/// precedence: a non-empty caller-supplied value (what the user typed
/// into the Decrypt prompt) wins; otherwise fall back to the keychain
/// entry written by the per-account "Unlock automatically" opt-in.  A
/// missing entry surfaces as `UnkaiError::Auth` so the receive bridge's
/// best-effort loader treats it as "no S/MIME passphrase available" and
/// leaves the identity unloaded.
pub fn resolve_smime_passphrase(account_id: &str, supplied: &str) -> Result<String, UnkaiError> {
    if !supplied.is_empty() {
        return Ok(supplied.to_string());
    }
    credentials::get_smime_passphrase(account_id)
}

/// Concrete `CryptoBridge` implementation used at the Tauri-command
/// boundary.  Holds the account's signing key (the user just unlocked
/// it via the passphrase prompt) plus a `Cache` handle for recipient
/// public-key lookups.  Short-lived: rebuilt per send / per fetch
/// because the passphrase shouldn't outlive one operation.
pub struct AppCryptoBridge {
    /// Pre-parsed and (logically) unlocked OpenPGP private key.  rpgp
    /// doesn't actually unlock until it needs the secret material, so
    /// this wrapper carries the passphrase too.  `None` on the receive
    /// path when the account has no PGP key configured (e.g. an
    /// S/MIME-only account) — the PGP trait methods then surface a clean
    /// `Auth` error rather than the bridge refusing to build.
    pub private_key: Option<unkai_crypto::PrivateKey>,
    /// The account's S/MIME identity (leaf cert + private key), parsed
    /// from the stored `.p12` (#338).  `None` for the PGP-only send path
    /// and whenever the account hasn't imported an S/MIME cert; the
    /// `decrypt_smime` method errors cleanly when it's missing.
    pub smime_identity: Option<unkai_crypto::CertificateWithKey>,
    /// Used to look up recipient public keys by email at encrypt
    /// time and trusted-signer keys at verify time.  Cheap to clone
    /// because `Cache` is an `Arc` internally.
    pub cache: Cache,
}

impl AppCryptoBridge {
    /// Build a PGP bridge from the account's stored armored key plus a
    /// freshly-prompted passphrase.  The caller is responsible for
    /// asking the user — we never read the passphrase from the
    /// keychain (the "re-prompt per operation" decision in #57).
    /// Returns `UnkaiError::Auth` when the keychain has no key entry
    /// for this account, so the IPC layer routes the user to the
    /// "set up encryption" flow rather than surfacing a raw error.
    ///
    /// This is the **send-side** constructor — it requires a PGP key
    /// because the only caller (the SMTP send path) is already gated on
    /// `encryption_mode == "pgp"`.  The receive path uses
    /// [`Self::for_account_receive`] instead, which loads whichever
    /// stacks the account has and tolerates either being absent.
    pub fn for_account(
        account_id: &str,
        passphrase: &str,
        cache: Cache,
    ) -> Result<Self, UnkaiError> {
        let armored = credentials::get_pgp_private_key(account_id)?;
        let private_key = unkai_crypto::parse_private_key(armored.as_bytes(), Some(passphrase))
            .map_err(|e| UnkaiError::Crypto(format!("Stored PGP key won't parse: {e}")))?;
        Ok(Self {
            private_key: Some(private_key),
            smime_identity: None,
            cache,
        })
    }

    /// Build an **S/MIME send-side** bridge (#338).  Sibling of
    /// [`Self::for_account`] for the S/MIME stack.
    ///
    /// `passphrase` is `Some` only when the send mode needs our own
    /// private identity (the `multipart/signed` sign-only path, and a
    /// future nested sign-then-encrypt): the `.p12` is loaded from the
    /// keychain and unlocked, with a wrong/missing passphrase surfacing
    /// as the usual `parse_pkcs12` error so the IPC layer can re-prompt.
    /// `None` is the **encrypt-only** case (`encryption_mode == "smime"`),
    /// which needs only the recipients' public certs — we don't load (or
    /// require) the account's own identity at all, so an account can
    /// encrypt to others before it has imported its own cert.
    pub fn for_account_smime_send(
        account_id: &str,
        passphrase: Option<&str>,
        cache: Cache,
    ) -> Result<Self, UnkaiError> {
        let smime_identity = match passphrase {
            Some(supplied) => {
                let p12 = credentials::get_smime_private_cert(account_id)?;
                let resolved = resolve_smime_passphrase(account_id, supplied)?;
                Some(unkai_crypto::parse_pkcs12(&p12, &resolved)?)
            }
            None => None,
        };
        Ok(Self {
            private_key: None,
            smime_identity,
            cache,
        })
    }

    /// Build a bridge for the **receive** path, loading every encryption
    /// stack the account has configured (#338).  Unlike
    /// [`Self::for_account`] this never fails on a missing identity — the
    /// receive path can't know whether an inbound message is PGP or
    /// S/MIME until it parses the envelope, so we load both best-effort
    /// and let the per-stack decrypt method surface a clean error if the
    /// message turns out to need a stack we couldn't load.
    ///
    /// `supplied_passphrase` is whatever the user typed into the Decrypt
    /// prompt (empty on the background-decrypt path).  It's tried against
    /// *both* stacks:
    /// - PGP: rpgp defers passphrase checking, so a wrong/empty value
    ///   parses fine here and only fails if a PGP message actually needs
    ///   decrypting with it.
    /// - S/MIME: PKCS#12 verifies the passphrase at parse time (MAC), so
    ///   a value that doesn't match the `.p12` simply leaves
    ///   `smime_identity = None`; that's correct, because a non-matching
    ///   passphrase means this typed value was for the other stack.
    ///
    /// Each stack falls back to its keychain "Unlock automatically"
    /// passphrase when `supplied_passphrase` is empty, via
    /// [`resolve_pgp_passphrase`] / [`resolve_smime_passphrase`].
    pub fn for_account_receive(account_id: &str, supplied_passphrase: &str, cache: Cache) -> Self {
        let private_key = (|| {
            let armored = credentials::get_pgp_private_key(account_id).ok()?;
            let passphrase = resolve_pgp_passphrase(account_id, supplied_passphrase).ok()?;
            match unkai_crypto::parse_private_key(armored.as_bytes(), Some(&passphrase)) {
                Ok(k) => Some(k),
                Err(e) => {
                    tracing::debug!(
                        "receive bridge: stored PGP key won't parse for '{account_id}': {e}"
                    );
                    None
                }
            }
        })();

        let smime_identity = (|| {
            let p12 = credentials::get_smime_private_cert(account_id).ok()?;
            let passphrase = resolve_smime_passphrase(account_id, supplied_passphrase).ok()?;
            match unkai_crypto::parse_pkcs12(&p12, &passphrase) {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::debug!(
                        "receive bridge: stored S/MIME cert won't unlock for '{account_id}': {e}"
                    );
                    None
                }
            }
        })();

        Self {
            private_key,
            smime_identity,
            cache,
        }
    }

    /// `true` when at least one encryption stack loaded — used by the
    /// background-decrypt path to skip the per-message fetch loop
    /// entirely when neither a PGP key nor an S/MIME identity is
    /// available (e.g. the user hasn't enabled "Unlock automatically"
    /// for either stack), exactly as the old PGP-only path bailed when
    /// the keychain passphrase didn't resolve.
    pub fn can_decrypt(&self) -> bool {
        self.private_key.is_some() || self.smime_identity.is_some()
    }

    /// The unlocked PGP key, or a clean `Auth` error when the account
    /// has none configured.  Centralised so the four PGP trait methods
    /// don't each repeat the `Option` unwrap, and so the error the IPC
    /// layer sees ("no PGP key") routes to the same "set up encryption"
    /// prompt as the missing-keychain-entry case.
    pub fn pgp_key(&self) -> Result<&unkai_crypto::PrivateKey, UnkaiError> {
        self.private_key
            .as_ref()
            .ok_or_else(|| UnkaiError::Auth("No PGP key is configured for this account".into()))
    }

    /// Resolve `recipient_emails` to the cached public keys we hold for
    /// each address.  Two-stage lookup:
    ///   1. The dedicated `pgp_public_keys` cache (fast path — hit on
    ///      any address whose key was imported via the AccountSettings
    ///      panel, the Compose paste flow, or the auto-import from a
    ///      vCard `KEY:` property on the last CardDAV sync).
    ///   2. Fallback: scan the `contacts` table for a vCard that has
    ///      this recipient as one of its emails *and* carries a
    ///      `KEY:` value.  Covers the case where the user added a
    ///      key directly via the contact form's Encryption section
    ///      but the post-save push into `pgp_public_keys` failed
    ///      silently (#57 follow-up — was the symptom that made this
    ///      fallback necessary in the first place).  On success the
    ///      key is best-effort upserted into `pgp_public_keys` so
    ///      the next send hits the fast path.
    ///
    /// Returns `CryptoKeyNotFound` only when *both* stages come up
    /// empty so the Compose layer can prompt the user to paste a key.
    pub fn collect_recipient_keys(
        &self,
        recipient_emails: &[String],
    ) -> Result<Vec<unkai_crypto::PublicKey>, UnkaiError> {
        let mut out = Vec::with_capacity(recipient_emails.len());
        for email in recipient_emails {
            // Stage 1 — fast path against pgp_public_keys.
            let rows = self
                .cache
                .get_pgp_public_keys_for_email(email)
                .map_err(UnkaiError::from)?;
            if let Some(row) = rows.into_iter().next() {
                let key = unkai_crypto::parse_public_key(row.armored_key.as_bytes())?;
                out.push(key);
                continue;
            }

            // Stage 2 — scan vCards.  `find_contact_vcards_with_email`
            // already filters down to vCards whose email list
            // contains the recipient, so this loop is bounded by
            // however many contacts share this address (typically 1).
            let vcards = self
                .cache
                .find_contact_vcards_with_email(email)
                .map_err(UnkaiError::from)?;
            let mut found: Option<unkai_crypto::PublicKey> = None;
            for vcard_raw in vcards {
                let parsed = match unkai_carddav::parse_vcard(&vcard_raw) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                for raw_key in parsed.keys {
                    let armored = match decode_vcard_key_value(&raw_key) {
                        Some(b) => b,
                        None => continue,
                    };
                    let key = match unkai_crypto::parse_public_key(&armored) {
                        Ok(k) => k,
                        Err(e) => {
                            tracing::warn!(
                                "Skipping unparseable PGP key on vCard for {email}: {e}"
                            );
                            continue;
                        }
                    };
                    // Best-effort warm the dedicated cache so the
                    // next send for this recipient hits stage 1.
                    let armored_string =
                        String::from_utf8(armored.clone()).unwrap_or_else(|_| String::new());
                    if !armored_string.is_empty() {
                        let _ = self.cache.upsert_pgp_public_key(&PgpPublicKeyRow {
                            fingerprint: key.fingerprint(),
                            email: Some(email.clone()),
                            armored_key: armored_string,
                            source: PgpKeySource::Vcard,
                            added_at: chrono::Utc::now().timestamp(),
                        });
                    }
                    found = Some(key);
                    break;
                }
                if found.is_some() {
                    break;
                }
            }
            match found {
                Some(key) => out.push(key),
                None => return Err(UnkaiError::CryptoKeyNotFound(email.clone())),
            }
        }
        Ok(out)
    }

    /// Resolve `recipient_emails` to the cached X.509 certificates we
    /// hold for each address (S/MIME counterpart to
    /// [`Self::collect_recipient_keys`], #338).  Two-stage lookup that
    /// mirrors the PGP path:
    ///
    ///   1. **Fast path** — the dedicated `smime_certs` cache (populated
    ///      by the `smime_import_public_cert` paste/file flow from
    ///      Chunk 2 and the vCard auto-import from Chunk 8).  The most
    ///      recently added cert for the address wins, matching how the
    ///      PGP fast path takes the first cached key.
    ///   2. **Fallback** — scan the `contacts` table for a vCard that
    ///      lists this recipient *and* carries an X.509 `KEY:` value.
    ///      Covers the window between adding a contact and the next
    ///      CardDAV sync's auto-import, and the case where that import
    ///      upsert failed silently.  On success the cert is best-effort
    ///      warmed into `smime_certs` so the next send hits stage 1.
    ///
    /// Returns `CryptoKeyNotFound` only when *both* stages come up empty,
    /// so Compose can prompt the user to import a cert.
    pub fn collect_recipient_smime_certs(
        &self,
        recipient_emails: &[String],
    ) -> Result<Vec<unkai_crypto::Certificate>, UnkaiError> {
        let mut out = Vec::with_capacity(recipient_emails.len());
        for email in recipient_emails {
            // Stage 1 — fast path against smime_certs.
            let rows = self
                .cache
                .get_smime_certs_for_email(email)
                .map_err(UnkaiError::from)?;
            if let Some(row) = rows.into_iter().next() {
                out.push(unkai_crypto::parse_der_cert(&row.der_cert)?);
                continue;
            }

            // Stage 2 — scan vCards whose email list contains this
            // recipient for an X.509 `KEY:` value.
            let vcards = self
                .cache
                .find_contact_vcards_with_email(email)
                .map_err(UnkaiError::from)?;
            let mut found: Option<unkai_crypto::Certificate> = None;
            'vcards: for vcard_raw in vcards {
                let parsed = match unkai_carddav::parse_vcard(&vcard_raw) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                for raw_key in parsed.keys {
                    let der_or_pem = match decode_vcard_smime_cert_value(&raw_key) {
                        Some(b) => b,
                        None => continue,
                    };
                    let cert = match unkai_crypto::parse_der_cert(&der_or_pem)
                        .or_else(|_| unkai_crypto::parse_pem_cert(&der_or_pem))
                    {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(
                                "Skipping unparseable S/MIME cert on vCard for {email}: {e}"
                            );
                            continue;
                        }
                    };
                    // Best-effort warm the dedicated cache so the next
                    // send for this recipient hits stage 1.  Bind under
                    // the address we were asked for so the lookup keys
                    // line up even if the cert's SAN differs.
                    if let Ok(der_cert) = cert.to_der() {
                        let _ = self.cache.upsert_smime_cert(&SmimeCertRow {
                            fingerprint: cert.fingerprint(),
                            email: Some(email.clone()),
                            der_cert,
                            source: SmimeCertSource::Vcard,
                            added_at: chrono::Utc::now().timestamp(),
                        });
                    }
                    found = Some(cert);
                    break 'vcards;
                }
            }
            match found {
                Some(cert) => out.push(cert),
                None => return Err(UnkaiError::CryptoKeyNotFound(email.clone())),
            }
        }
        Ok(out)
    }

    /// Gather the X.509 certs we already hold for an inbound message's
    /// sender — the TOFU candidate set for [`unkai_crypto::smime_verify`]
    /// (#338 trust model).  `sender_from` is the raw `From` value
    /// (`"Name <addr>"` or a bare address); we extract the address and
    /// look it up in the `smime_certs` cache.
    ///
    /// Unlike [`Self::collect_recipient_smime_certs`], a sender with no
    /// cached cert is **not** an error — it just means we can't establish
    /// TOFU trust, and the verifier falls back to CA-chain or amber
    /// "untrusted issuer".  Unparseable cached rows are skipped rather
    /// than failing the whole verify.
    pub fn collect_sender_smime_certs(
        &self,
        sender_from: &str,
    ) -> Result<Vec<unkai_crypto::Certificate>, UnkaiError> {
        let address = extract_bare_address(sender_from);
        let rows = self
            .cache
            .get_smime_certs_for_email(&address)
            .map_err(UnkaiError::from)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            match unkai_crypto::parse_der_cert(&row.der_cert) {
                Ok(cert) => out.push(cert),
                Err(e) => tracing::warn!(
                    "Skipping unparseable cached S/MIME cert fp={} for sender verify: {e}",
                    row.fingerprint
                ),
            }
        }
        Ok(out)
    }

    /// Materialise every cached public key as a trust set for
    /// `decrypt_and_verify`.  Cheap because the cache returns plain
    /// armored strings — rpgp does the parse work.  Errors on
    /// individual rows are logged and skipped rather than failing
    /// the whole decrypt: a malformed cached key shouldn't block
    /// the user from reading the message.
    pub fn collect_all_trusted_keys(&self) -> Result<Vec<unkai_crypto::PublicKey>, UnkaiError> {
        let rows = self
            .cache
            .list_pgp_public_keys()
            .map_err(UnkaiError::from)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            match unkai_crypto::parse_public_key(row.armored_key.as_bytes()) {
                Ok(k) => out.push(k),
                Err(e) => tracing::warn!(
                    "Skipping cached public key fp={} (parse failed): {e}",
                    row.fingerprint
                ),
            }
        }
        Ok(out)
    }
}

impl unkai_core::crypto::CryptoBridge for AppCryptoBridge {
    fn decrypt(
        &self,
        ciphertext_armor: &[u8],
    ) -> Result<unkai_core::crypto::DecryptedPayload, UnkaiError> {
        let trusted = self.collect_all_trusted_keys()?;
        let trusted_refs: Vec<&unkai_crypto::PublicKey> = trusted.iter().collect();
        let result =
            unkai_crypto::decrypt_and_verify(ciphertext_armor, self.pgp_key()?, &trusted_refs)?;
        Ok(unkai_core::crypto::DecryptedPayload {
            plaintext: result.plaintext,
            signature_status: result.signature_status.map(serialize_signature_status),
            signer_fingerprint: result.signer_fingerprint,
        })
    }

    fn decrypt_smime(
        &self,
        cms_der: &[u8],
    ) -> Result<unkai_core::crypto::DecryptedPayload, UnkaiError> {
        let identity = self.smime_identity.as_ref().ok_or_else(|| {
            UnkaiError::Auth("No S/MIME certificate is configured for this account".into())
        })?;
        let result = unkai_crypto::smime_decrypt(cms_der, identity)?;
        Ok(unkai_core::crypto::DecryptedPayload {
            plaintext: result.plaintext,
            // `signature_status` is always `None` from `smime_decrypt`
            // today (the nested sign-then-encrypt form is a follow-up);
            // map it through the same serializer the PGP path uses so the
            // two stacks stay wire-identical the moment it lands.
            signature_status: result.signature_status.map(serialize_signature_status),
            // S/MIME attributes the signer by subject DN rather than a
            // fingerprint; until the nested-signature path is wired there
            // is nothing to surface here.
            signer_fingerprint: None,
        })
    }

    fn verify(
        &self,
        signed_payload: &[u8],
        signature_armor: &[u8],
    ) -> Result<unkai_core::crypto::VerifyOutcome, UnkaiError> {
        let trusted = self.collect_all_trusted_keys()?;
        let trusted_refs: Vec<&unkai_crypto::PublicKey> = trusted.iter().collect();
        let (status, signer_fingerprint) =
            unkai_crypto::verify_detached(signed_payload, signature_armor, &trusted_refs)?;
        Ok(unkai_core::crypto::VerifyOutcome {
            status: serialize_signature_status(status),
            signer_fingerprint,
        })
    }

    fn verify_smime(
        &self,
        signed_payload: &[u8],
        signature_der: &[u8],
        sender_from: &str,
    ) -> Result<unkai_core::crypto::VerifyOutcome, UnkaiError> {
        // The bundled Mozilla roots are the chain-trust anchors.  Built
        // per call to sidestep stashing a (non-`Clone`) `X509Store` on the
        // bridge; the ~150 DER parses are cheap next to fetching the mail,
        // and verification only runs on the signed-message minority.
        let trust_store = unkai_crypto::build_mozilla_trust_store()?;
        let candidates = self.collect_sender_smime_certs(sender_from)?;
        let candidate_refs: Vec<&unkai_crypto::Certificate> = candidates.iter().collect();
        let outcome = unkai_crypto::smime_verify(
            signed_payload,
            signature_der,
            &candidate_refs,
            &trust_store,
        )?;
        Ok(unkai_core::crypto::VerifyOutcome {
            status: serialize_signature_status(outcome.status),
            signer_fingerprint: outcome.signer_fingerprint,
        })
    }

    fn encrypt(
        &self,
        inner_mime: &[u8],
        recipient_emails: &[String],
        sign: bool,
    ) -> Result<unkai_core::crypto::EncryptedOutput, UnkaiError> {
        let recipient_keys = self.collect_recipient_keys(recipient_emails)?;
        let recipient_refs: Vec<&unkai_crypto::PublicKey> = recipient_keys.iter().collect();
        let armored = if sign {
            unkai_crypto::sign_and_encrypt(inner_mime, self.pgp_key()?, &recipient_refs)?
        } else {
            unkai_crypto::encrypt(inner_mime, &recipient_refs)?
        };
        Ok(unkai_core::crypto::EncryptedOutput {
            ciphertext_armor: armored,
        })
    }

    fn sign(&self, signed_payload: &[u8]) -> Result<Vec<u8>, UnkaiError> {
        unkai_crypto::sign_detached(signed_payload, self.pgp_key()?)
    }

    fn encrypt_smime(
        &self,
        inner_mime: &[u8],
        recipient_emails: &[String],
    ) -> Result<Vec<u8>, UnkaiError> {
        let certs = self.collect_recipient_smime_certs(recipient_emails)?;
        let cert_refs: Vec<&unkai_crypto::Certificate> = certs.iter().collect();
        unkai_crypto::smime_encrypt(inner_mime, &cert_refs)
    }

    fn sign_smime(&self, signed_payload: &[u8]) -> Result<Vec<u8>, UnkaiError> {
        let identity = self.smime_identity.as_ref().ok_or_else(|| {
            UnkaiError::Auth("No S/MIME certificate is configured for this account".into())
        })?;
        unkai_crypto::smime_sign(signed_payload, identity)
    }
}

/// Pull the bare address out of a `From`-style value: `"Name <a@b>"`
/// → `"a@b"`, a bare `"a@b"` unchanged.  Keyed verbatim (no case
/// folding) into the `smime_certs` lookup, matching how the recipient
/// path passes addresses straight through.
pub fn extract_bare_address(from: &str) -> String {
    match (from.rfind('<'), from.rfind('>')) {
        (Some(start), Some(end)) if start < end => from[start + 1..end].trim().to_string(),
        _ => from.trim().to_string(),
    }
}

/// Convert the typed `unkai_crypto::SignatureStatus` enum to the
/// kebab-case string the rest of the workspace (cache columns, JSON
/// IPC payload, Svelte UI) consumes.  Single source of truth so the
/// strings don't drift between Rust and TypeScript.
pub fn serialize_signature_status(status: unkai_crypto::SignatureStatus) -> String {
    match status {
        unkai_crypto::SignatureStatus::Valid => "valid".into(),
        unkai_crypto::SignatureStatus::Invalid => "invalid".into(),
        unkai_crypto::SignatureStatus::UnknownSigner => "unknown-signer".into(),
        unkai_crypto::SignatureStatus::ValidUntrustedIssuer => "valid-untrusted-issuer".into(),
        unkai_crypto::SignatureStatus::ValidExpiredCert => "valid-expired-cert".into(),
    }
}

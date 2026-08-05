//! CardDAV contacts, groups, categories, and mailing lists.
//!
//! Mirrors `ui/src/lib/api/contacts.ts`.

use serde::Deserialize;
use serde::Serialize;
use unkai_carddav::Addressbook;
use unkai_carddav::ParsedVcard;
use unkai_carddav::RawContact;
use unkai_carddav::build_vcard;
use unkai_carddav::list_addressbooks_at;
use unkai_carddav::sync_addressbook;
use unkai_core::UnkaiError;
use unkai_core::models::Contact;
use unkai_store::Cache;
use unkai_store::cache::ContactRow;
use unkai_store::cache::ContactServerHandle;
use unkai_store::cache::PgpKeySource;
use unkai_store::cache::PgpPublicKeyRow;
use unkai_store::cache::SmimeCertRow;
use unkai_store::cache::SmimeCertSource;
use unkai_store::credentials;
use unkai_store::nextcloud_store;

use crate::nextcloud::list_nextcloud_groups;
use crate::support::{
    LOCAL_ADDRESSBOOK_NAME, SyncStatus, carddav_home_of, dav_create_contact_for,
    dav_delete_contact_for, dav_update_contact_for, load_nextcloud_account, url_origin,
};

// ── CardDAV contacts ────────────────────────────────────────────
//
// Contact sync is driven from a single entry point: the UI calls
// `sync_nextcloud_contacts(nc_id)` (a "Sync now" button in settings,
// or a background tick after login). That command walks the user's
// addressbooks, runs one incremental sync per book via sync-collection
// REPORT, and applies each delta to the local cache transactionally.
//
// The UI never sees hrefs, etags, or sync tokens — it reads fully
// hydrated `Contact` records from the cache via `get_contacts` (list
// view) and `search_contacts` (autocomplete).

/// Summary returned to the UI after a contacts sync run.
///
/// Per-addressbook counts let the UI say something more useful than
/// "done" — e.g. "Contacts: 12 new, 1 removed". `errors` carries the
/// list of addressbooks that failed so the overall sync doesn't look
/// green when one book silently fell over.
#[derive(Debug, Clone, Serialize)]
pub struct SyncContactsReport {
    pub nc_account_id: String,
    pub books_synced: u32,
    pub upserted: u32,
    pub deleted: u32,
    pub errors: Vec<String>,
}

/// Pull the latest contacts from a Nextcloud account.
///
/// Two-step: list addressbooks (PROPFIND on the user's home), then
/// run an incremental sync-collection REPORT against each. Each
/// addressbook's delta is committed to the local cache in its own
/// transaction, so a failure on book N+1 doesn't roll back book N.
/// Per-book errors are logged and accumulated into the report rather
/// than aborting the whole run.
pub async fn sync_nextcloud_contacts(
    nc_id: String,
    cache: &Cache,
) -> Result<SyncContactsReport, UnkaiError> {
    let account = nextcloud_store::load_accounts(cache)?
        .into_iter()
        .find(|a| a.id == nc_id)
        .ok_or_else(|| UnkaiError::Other(format!("no Nextcloud account with id '{nc_id}'")))?;

    let mut report = SyncContactsReport {
        nc_account_id: nc_id.clone(),
        books_synced: 0,
        upserted: 0,
        deleted: 0,
        errors: Vec::new(),
    };

    // A local-only source has nothing to sync with (#413) — the
    // cache *is* the source of truth. Empty report, no error.
    if account.is_local() {
        return Ok(report);
    }
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    let books = list_addressbooks_at(
        &carddav_home_of(&account),
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    tracing::info!(
        "CardDAV: {} addressbook(s) to sync for {}",
        books.len(),
        nc_id
    );

    for book in books {
        // Prior token (if any) makes the REPORT incremental; missing
        // state means first sync and the CardDAV layer handles that.
        let prev_token = cache
            .get_addressbook_sync_state(&nc_id, &book.name)
            .ok()
            .flatten()
            .and_then(|s| s.sync_token);

        // Base for resolving hrefs in the sync response. For a
        // generic DAV source the typed server URL may carry a path
        // (e.g. https://host/dav.php), so use the collection's own
        // origin; for Nextcloud the server URL already is an origin.
        let delta = match sync_addressbook(
            &url_origin(&book.path),
            &book.path,
            &account.username,
            &app_password,
            prev_token.as_deref(),
            &account.trusted_certs,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("CardDAV sync failed for book '{}': {e}", book.name);
                report.errors.push(format!("{}: {e}", book.name));
                continue;
            }
        };

        let upserts: Vec<ContactRow> = delta.upserts.iter().map(raw_contact_to_row).collect();

        if let Err(e) = cache.apply_contact_delta(
            &nc_id,
            &book.name,
            book.display_name.as_deref(),
            &upserts,
            &delta.deleted_hrefs,
            delta.new_sync_token.as_deref(),
            book.ctag.as_deref(),
        ) {
            tracing::warn!("apply_contact_delta failed for '{}': {e}", book.name);
            report.errors.push(format!("{}: {e}", book.name));
            continue;
        }

        // Auto-import OpenPGP keys carried by `KEY:` properties on the
        // freshly-synced vCards into the recipient-key cache (#57, #339).
        // Best-effort — a malformed key on one contact shouldn't fail the
        // whole sync; we log and continue per-contact.
        auto_import_pgp_keys(cache, &delta.upserts);
        // Same pass for X.509 certs on those `KEY:` properties → the
        // S/MIME recipient-cert cache (#338).  The two importers split
        // the same `KEY:` values by media type, so a contact carrying
        // both a PGP key and an S/MIME cert lands in both caches.
        auto_import_smime_certs(cache, &delta.upserts);

        report.books_synced += 1;
        report.upserted += upserts.len() as u32;
        report.deleted += delta.deleted_hrefs.len() as u32;
    }

    Ok(report)
}

/// Walk freshly-synced vCards, pull out any `KEY:` values, and upsert
/// them into the recipient-key cache (#57).
///
/// Supported source forms:
///   - `data:application/pgp-keys;base64,…` (Autocrypt + the form
///     Nextcloud Contacts emits).
///   - Inline ASCII-armored key (rare but legal; some MUAs emit it).
///   - Plain `https://…` URL: skipped here — we don't fetch keys
///     out-of-band; a future PR can add keyserver lookup behind a
///     user-visible toggle.
///
/// Each successfully-parsed key round-trips through
/// `unkai_crypto::parse_public_key` for self-signature validation
/// before it lands in the cache.  Bogus blobs are logged and dropped
/// — better to skip one contact's key than to refuse to sync the
/// whole addressbook.
pub fn auto_import_pgp_keys(cache: &Cache, raw_contacts: &[RawContact]) {
    use base64::Engine;

    for contact in raw_contacts {
        if contact.keys.is_empty() {
            continue;
        }
        let primary_email = contact.emails.first().map(|e| e.value.clone());
        for raw_key in &contact.keys {
            let armored = match decode_vcard_key_value(raw_key) {
                Some(bytes) => bytes,
                None => continue,
            };
            let parsed = match unkai_crypto::parse_public_key(&armored) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        "Skipping unparseable PGP key on vCard {}: {e}",
                        contact.vcard_uid
                    );
                    continue;
                }
            };
            let row = PgpPublicKeyRow {
                fingerprint: parsed.fingerprint(),
                email: primary_email.clone(),
                armored_key: String::from_utf8(armored.clone()).unwrap_or_else(|_| {
                    // The key parsed but came in as binary — re-armor it
                    // through the standard form so the cache always
                    // stores ASCII.  Fall back to base64 of the raw
                    // bytes if even that fails; the lookup is by
                    // fingerprint so the armor is purely for export.
                    base64::engine::general_purpose::STANDARD.encode(&armored)
                }),
                source: PgpKeySource::Vcard,
                added_at: chrono::Utc::now().timestamp(),
            };
            if let Err(e) = cache.upsert_pgp_public_key(&row) {
                tracing::warn!(
                    "Failed to cache PGP key fp={} for vCard {}: {e}",
                    row.fingerprint,
                    contact.vcard_uid
                );
            }
        }
    }
}

/// Walk freshly-synced vCards, pull out any X.509 certificates carried
/// by `KEY:` properties, and upsert them into the recipient-cert cache
/// (#338 — the S/MIME counterpart to [`auto_import_pgp_keys`]).
///
/// The same `KEY:` property holds both stacks' material;
/// [`decode_vcard_smime_cert_value`] selects only the X.509 entries by
/// their data-URI media type (`application/x-x509-user-cert`,
/// `application/pkcs7-mime`, …) so an OpenPGP key on the same contact
/// is left for [`auto_import_pgp_keys`] and vice versa.
///
/// **Email binding:** the cert's own Subject Alternative Name
/// `rfc822Name` is the authoritative S/MIME address binding (RFC 8551
/// §3, anchor 6), so we prefer it for the `email` column and only fall
/// back to the contact's primary email when the cert carries no SAN
/// email (a non-conformant cert — better findable under the contact's
/// address than orphaned).  This is the reverse priority from the
/// manual `smime_import_public_cert` paste flow, where the user-typed
/// hint wins; here there's no hint, just the contact card.
///
/// Best-effort — a malformed cert on one contact is logged and skipped
/// rather than failing the whole sync, exactly like the PGP path.
pub fn auto_import_smime_certs(cache: &Cache, raw_contacts: &[RawContact]) {
    for contact in raw_contacts {
        if contact.keys.is_empty() {
            continue;
        }
        let primary_email = contact.emails.first().map(|e| e.value.clone());
        for raw_key in &contact.keys {
            let der_or_pem = match decode_vcard_smime_cert_value(raw_key) {
                Some(bytes) => bytes,
                None => continue,
            };
            // `parse_smime_cert_flexible` takes a string (PEM text or
            // base64) but the vCard payload is already raw DER/PEM
            // bytes, so go straight through the byte-level parsers:
            // DER first (the common data-URI form), then PEM (inline
            // `-----BEGIN CERTIFICATE-----`).
            let cert = match unkai_crypto::parse_der_cert(&der_or_pem)
                .or_else(|_| unkai_crypto::parse_pem_cert(&der_or_pem))
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "Skipping unparseable S/MIME cert on vCard {}: {e}",
                        contact.vcard_uid
                    );
                    continue;
                }
            };
            let der_cert = match cert.to_der() {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(
                        "Skipping S/MIME cert on vCard {} — re-encode failed: {e}",
                        contact.vcard_uid
                    );
                    continue;
                }
            };
            let row = SmimeCertRow {
                fingerprint: cert.fingerprint(),
                email: cert.email().or_else(|| primary_email.clone()),
                der_cert,
                source: SmimeCertSource::Vcard,
                added_at: chrono::Utc::now().timestamp(),
            };
            if let Err(e) = cache.upsert_smime_cert(&row) {
                tracing::warn!(
                    "Failed to cache S/MIME cert fp={} for vCard {}: {e}",
                    row.fingerprint,
                    contact.vcard_uid
                );
            }
        }
    }
}

/// What kind of cryptographic material a vCard `KEY:` value carries,
/// after we've stripped its data-URI / inline-armor wrapper.
///
/// A single `KEY:` property can hold either an OpenPGP public key
/// (#57) or an X.509 certificate (#338) — RFC 6350 §6.8.1 reuses the
/// one property for both, distinguishing them by the data-URI media
/// type.  Classifying once and routing to the right cache keeps the
/// PGP and S/MIME auto-import paths from each having to parse-and-fail
/// on the other stack's material on every contact sync.
pub enum VcardKeyMaterial {
    /// OpenPGP public-key packets — armored ASCII or binary octet
    /// stream, ready for `unkai_crypto::parse_public_key`.
    Pgp(Vec<u8>),
    /// X.509 certificate — DER or PEM bytes, ready for the S/MIME cert
    /// parser (`parse_der_cert` / `parse_pem_cert`).
    Smime(Vec<u8>),
}

/// Classify a vCard `KEY:` property value and decode its payload.
///
/// vCard `KEY:` material arrives in a handful of shapes:
///   - **Inline armor** — `-----BEGIN PGP PUBLIC KEY BLOCK-----…`
///     (PGP) or `-----BEGIN CERTIFICATE-----…` (X.509 PEM).
///   - **`data:` URI** — `data:<media-type>;base64,<payload>`.  The
///     media type routes the stack:
///       * `application/pgp-keys` (Autocrypt) or *no type* → PGP.  The
///         untyped default stays PGP for backward-compat with the #57
///         import, which predates any X.509 support.
///       * `application/x-x509-user-cert` / `application/x-x509-ca-cert`
///         / `application/pkix-cert` / `application/pkcs7-mime` → X.509.
///         These are the media types CardDAV servers emit for S/MIME
///         certs.
///
/// Returns `None` for forms we don't ingest inline — bare HTTP/HTTPS
/// URLs (which would need an out-of-band keyserver/directory fetch,
/// a future follow-up) and malformed data URIs — so callers skip them
/// cleanly rather than emitting a hard error.
///
/// The vCard writer in `unkai_carddav` runs `KEY:` values through the
/// RFC 6350 §3.4 text-escape pass (`\\`, `\n`, `\,`, `\;`).  The
/// upstream ical parser surfaces the *escaped* form unchanged, so the
/// first thing we do is unescape — without it an inline armored block
/// round-trips as `…\\n\\n<base64>\\n…` and the armor parser fails on
/// the `\n` literal where it expects a real CRLF; same story for a
/// `data:` URI whose `;base64,` separators got escaped on the way out.
pub fn classify_vcard_key_value(value: &str) -> Option<VcardKeyMaterial> {
    use base64::Engine;

    let unescaped = unescape_vcard_text(value);
    let trimmed = unescaped.trim();

    // Inline armored ASCII — pass through unchanged, routed by the
    // armor header.
    if trimmed.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----") {
        return Some(VcardKeyMaterial::Pgp(trimmed.as_bytes().to_vec()));
    }
    if trimmed.starts_with("-----BEGIN CERTIFICATE-----") {
        return Some(VcardKeyMaterial::Smime(trimmed.as_bytes().to_vec()));
    }

    // `data:` URI form.  Split the media-type header from the payload
    // and route by media type.
    if let Some(rest) = trimmed.strip_prefix("data:") {
        let comma = rest.find(',')?;
        let header = &rest[..comma];
        let payload = &rest[comma + 1..];

        // Decode the payload to raw bytes.  `;base64,` is the
        // overwhelmingly common encoding; `data:,…` without base64 is
        // rare for key material and we pass it through verbatim
        // (without decoding %xx escapes).
        let bytes = if header.contains("base64") {
            base64::engine::general_purpose::STANDARD
                .decode(payload.as_bytes())
                .ok()?
        } else {
            payload.as_bytes().to_vec()
        };

        // X.509 media types → S/MIME.  Matched case-insensitively
        // because CardDAV servers aren't consistent about casing.
        let header_lower = header.to_ascii_lowercase();
        if header_lower.contains("x509")
            || header_lower.contains("pkix-cert")
            || header_lower.contains("pkcs7")
        {
            return Some(VcardKeyMaterial::Smime(bytes));
        }

        // `application/pgp-keys` or an untyped data URI → PGP (the
        // historical #57 default).
        return Some(VcardKeyMaterial::Pgp(bytes));
    }

    // HTTP/HTTPS reference — out-of-band fetch is a follow-up.
    None
}

/// Decode a vCard `KEY:` value into the OpenPGP byte blob that
/// `unkai_crypto::parse_public_key` can ingest, or `None` if the value
/// isn't OpenPGP material (an X.509 cert, an unfetchable URL, or a
/// malformed data URI).  Thin PGP-only view over
/// [`classify_vcard_key_value`].
pub fn decode_vcard_key_value(value: &str) -> Option<Vec<u8>> {
    match classify_vcard_key_value(value) {
        Some(VcardKeyMaterial::Pgp(bytes)) => Some(bytes),
        _ => None,
    }
}

/// Decode a vCard `KEY:` value into the X.509 certificate byte blob
/// (DER or PEM) that the S/MIME cert parser can ingest, or `None` if
/// the value isn't X.509 material (an OpenPGP key, an unfetchable URL,
/// or a malformed data URI).  S/MIME counterpart to
/// [`decode_vcard_key_value`] (#338).
pub fn decode_vcard_smime_cert_value(value: &str) -> Option<Vec<u8>> {
    match classify_vcard_key_value(value) {
        Some(VcardKeyMaterial::Smime(bytes)) => Some(bytes),
        _ => None,
    }
}

/// Unescape RFC 6350 §3.4 vCard text-value escape sequences:
///
///   `\\` → `\`,  `\n` or `\N` → newline,  `\,` → `,`,  `\;` → `;`
///
/// Unknown `\<char>` escapes are preserved verbatim so we don't
/// silently lose data on a malformed input; a lone trailing `\` is
/// also preserved.  Idempotent on already-unescaped strings —
/// none of the escape-pair forms appear in pure base64 or
/// armored OpenPGP content (the armored format uses `=`, `+`, `/`,
/// real `\n` LFs, never `\<char>` pairs).
pub fn unescape_vcard_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(',') => out.push(','),
            Some(';') => out.push(';'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
pub mod vcard_key_decode_tests {
    use super::{decode_vcard_key_value, decode_vcard_smime_cert_value, unescape_vcard_text};

    #[test]
    fn unescape_round_trips_armored_newlines() {
        // The vCard writer turns real newlines into `\n` literals.
        // Unescape must turn them back so rpgp sees a clean
        // PEM-style block.
        let escaped = "-----BEGIN PGP PUBLIC KEY BLOCK-----\\n\\nABCD\\n-----END PGP PUBLIC KEY BLOCK-----\\n";
        let got = unescape_vcard_text(escaped);
        assert_eq!(
            got,
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nABCD\n-----END PGP PUBLIC KEY BLOCK-----\n"
        );
    }

    #[test]
    fn unescape_preserves_double_backslash_n() {
        // A literal backslash followed by `n` must NOT collapse to a
        // newline — that's `\\` + `n`, two characters, which the
        // writer emits as `\\\\n` (four chars: backslash, backslash,
        // backslash, n).  After one unescape pass we get the
        // original literal `\n` (backslash + n).
        assert_eq!(unescape_vcard_text("\\\\n"), "\\n");
    }

    #[test]
    fn unescape_handles_data_uri_separators() {
        // `data:application/pgp-keys;base64,…` writes with `\;` and
        // `\,` escapes; unescape restores the raw URI form.
        let escaped = "data:application/pgp-keys\\;base64\\,AAAA";
        assert_eq!(
            unescape_vcard_text(escaped),
            "data:application/pgp-keys;base64,AAAA"
        );
    }

    #[test]
    fn unescape_lone_trailing_backslash_is_preserved() {
        assert_eq!(unescape_vcard_text("abc\\"), "abc\\");
    }

    #[test]
    fn unescape_unknown_escape_is_preserved() {
        // `\?` isn't a recognised escape; emit both characters
        // verbatim rather than swallowing the `?`.
        assert_eq!(unescape_vcard_text("a\\?b"), "a\\?b");
    }

    #[test]
    fn decode_armored_with_escapes_yields_clean_bytes() {
        // End-to-end: the value the carddav layer hands us has
        // escaped newlines, but the bytes we return to
        // `unkai_crypto::parse_public_key` must have real `\n`s.
        let escaped = "-----BEGIN PGP PUBLIC KEY BLOCK-----\\n\\nABCD\\n-----END PGP PUBLIC KEY BLOCK-----\\n";
        let bytes = decode_vcard_key_value(escaped).expect("must decode");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            s.contains("\n\n"),
            "armored body must contain real newlines, got: {s:?}"
        );
    }

    #[test]
    fn decode_data_uri_with_escaped_separators_decodes_base64() {
        // Same `data:` shape Nextcloud Contacts writes, escaped
        // through the vCard layer.  Base64 of `hello` is `aGVsbG8=`.
        let escaped = "data:application/pgp-keys\\;base64\\,aGVsbG8=";
        let bytes = decode_vcard_key_value(escaped).expect("must decode");
        assert_eq!(bytes, b"hello");
    }

    // --- S/MIME (X.509) routing (#338) -----------------------------------

    #[test]
    fn smime_decoder_takes_x509_user_cert_data_uri() {
        // `application/x-x509-user-cert` is the media type CardDAV
        // servers emit for an S/MIME cert.  Base64 of `cert!` is
        // `Y2VydCE=`.
        let escaped = "data:application/x-x509-user-cert\\;base64\\,Y2VydCE=";
        let bytes = decode_vcard_smime_cert_value(escaped).expect("must decode as S/MIME");
        assert_eq!(bytes, b"cert!");
    }

    #[test]
    fn smime_decoder_takes_pkcs7_and_pkix_cert_data_uris() {
        // The other two X.509 media types we accept.  Base64 of `x` is
        // `eA==`.
        for header in ["application/pkcs7-mime", "application/pkix-cert"] {
            let uri = format!("data:{header};base64,eA==");
            let bytes = decode_vcard_smime_cert_value(&uri)
                .unwrap_or_else(|| panic!("{header} must route to S/MIME"));
            assert_eq!(bytes, b"x");
        }
    }

    #[test]
    fn smime_decoder_takes_inline_certificate_pem() {
        // Inline PEM cert routes to the S/MIME decoder, not the PGP one.
        let pem = "-----BEGIN CERTIFICATE-----\\nQUJDRA==\\n-----END CERTIFICATE-----\\n";
        let bytes = decode_vcard_smime_cert_value(pem).expect("must decode inline PEM cert");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(s.contains('\n'), "escaped newlines must be restored");
    }

    #[test]
    fn the_two_decoders_are_mutually_exclusive() {
        // A PGP key value yields PGP bytes but NOT S/MIME bytes, and an
        // X.509 cert value yields S/MIME bytes but NOT PGP bytes — so
        // the two auto-import passes never double-process one `KEY:`.
        let pgp = "data:application/pgp-keys;base64,aGVsbG8=";
        assert!(decode_vcard_key_value(pgp).is_some());
        assert!(decode_vcard_smime_cert_value(pgp).is_none());

        let smime = "data:application/x-x509-user-cert;base64,Y2VydCE=";
        assert!(decode_vcard_smime_cert_value(smime).is_some());
        assert!(decode_vcard_key_value(smime).is_none());
    }

    #[test]
    fn untyped_data_uri_stays_pgp_for_backcompat() {
        // An untyped `data:;base64,…` predates X.509 support and must
        // keep routing to PGP (the #57 default) — not silently divert
        // existing recipient keys into the cert cache.
        let untyped = "data:;base64,aGVsbG8=";
        assert!(decode_vcard_key_value(untyped).is_some());
        assert!(decode_vcard_smime_cert_value(untyped).is_none());
    }
}

/// Cache-only list of contacts, optionally scoped to a single NC account.
pub fn get_contacts(nc_id: Option<String>, cache: &Cache) -> Result<Vec<Contact>, UnkaiError> {
    cache.list_contacts(nc_id.as_deref()).map_err(Into::into)
}

/// Substring search over cached contacts — feeds the compose
/// autocomplete dropdown. `limit` caps the row count so a stray
/// single-character query can't return the whole address book.
pub fn search_contacts(
    query: String,
    limit: u32,
    cache: &Cache,
) -> Result<Vec<Contact>, UnkaiError> {
    cache.search_contacts(&query, limit).map_err(Into::into)
}

pub fn get_contacts_sync_status(nc_id: String, cache: &Cache) -> Result<SyncStatus, UnkaiError> {
    let last = cache
        .latest_addressbook_sync_at(&nc_id)
        .map_err(UnkaiError::from)?
        .map(|t| t.to_rfc3339());
    let count = cache.count_contacts(&nc_id).map_err(UnkaiError::from)?;
    Ok(SyncStatus {
        last_synced_at: last,
        count,
    })
}

/// Fetched separately from `get_contacts` because photo bytes are
/// huge and Tauri serialises them as JSON number arrays — shipping
/// every photo with the list payload made the contacts view feel
/// laggy. The UI requests photos only for rows it actually paints.
#[derive(Debug, Clone, Serialize)]
pub struct ContactPhoto {
    pub mime: String,
    pub data: Vec<u8>,
}

pub fn get_contact_photo(
    contact_id: String,
    cache: &Cache,
) -> Result<Option<ContactPhoto>, UnkaiError> {
    Ok(cache
        .get_contact_photo(&contact_id)
        .map_err(UnkaiError::from)?
        .map(|(mime, data)| ContactPhoto { mime, data }))
}

/// Field-for-field copy between the CardDAV crate's `RawContact` and
/// the store crate's `ContactRow`. Kept as a free function so neither
/// crate has to depend on the other — the Tauri layer is the only
/// place both are in scope.
pub fn raw_contact_to_row(c: &RawContact) -> ContactRow {
    ContactRow {
        href: c.href.clone(),
        etag: c.etag.clone(),
        vcard_uid: c.vcard_uid.clone(),
        display_name: c.display_name.clone(),
        emails: c
            .emails
            .iter()
            .map(|e| unkai_core::models::ContactEmail {
                kind: e.kind.clone(),
                value: e.value.clone(),
            })
            .collect(),
        phones: c
            .phones
            .iter()
            .map(|p| unkai_core::models::ContactPhone {
                kind: p.kind.clone(),
                value: p.value.clone(),
            })
            .collect(),
        organization: c.organization.clone(),
        photo_mime: c.photo_mime.clone(),
        photo_data: c.photo_data.clone(),
        title: c.title.clone(),
        birthday: c.birthday.clone(),
        note: c.note.clone(),
        addresses: c
            .addresses
            .iter()
            .map(|a| unkai_core::models::ContactAddress {
                kind: a.kind.clone(),
                street: a.street.clone(),
                locality: a.locality.clone(),
                region: a.region.clone(),
                postal_code: a.postal_code.clone(),
                country: a.country.clone(),
            })
            .collect(),
        urls: c.urls.clone(),
        vcard_raw: c.vcard_raw.clone(),
        kind: c.kind.clone(),
        member_uids: c.member_uids.clone(),
        categories: c.categories.clone(),
    }
}

// ── CardDAV writes (create / update / delete) ───────────────────
//
// These three commands are the UI's entry points for editing
// contacts. They each do the same three-step dance:
//
// 1. Build a vCard 4.0 body from the form input.
// 2. PUT / DELETE against the CardDAV server with the right
//    precondition (If-None-Match for create, If-Match for
//    update/delete) so conflicting writes surface as a structured
//    error rather than silently clobbering remote state.
// 3. Write through to the local cache so the UI reflects the
//    change immediately — we don't wait for the next sync tick.
//
// For update/delete we look up the server bookkeeping (href, etag,
// addressbook) by contact id; the UI never has to carry those around.

/// Editable fields for a contact, shared by create and update.
/// The "extended" block (title, birthday, note, addresses, urls)
/// is optional so older UI versions that don't surface those
/// fields keep working — `update_contact` merges over the cached
/// vCard, so missing fields preserve whatever's on the server
/// instead of clobbering it.
#[derive(Debug, Clone, Deserialize)]
pub struct ContactInput {
    pub display_name: String,
    pub emails: Vec<unkai_core::models::ContactEmail>,
    pub phones: Vec<unkai_core::models::ContactPhone>,
    pub organization: Option<String>,
    pub photo_mime: Option<String>,
    pub photo_data: Option<Vec<u8>>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub birthday: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub addresses: Option<Vec<unkai_core::models::ContactAddress>>,
    #[serde(default)]
    pub urls: Option<Vec<String>>,
    // ── #143: vCard 4 fields surfaced in the redesigned form ─────
    #[serde(default)]
    pub structured_name: Option<unkai_core::models::StructuredName>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub anniversary: Option<String>,
    #[serde(default)]
    pub gender: Option<String>,
    #[serde(default)]
    pub impp: Option<Vec<unkai_core::models::ContactImpp>>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    #[serde(default)]
    pub geo: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    /// Round-tripped through the vCard's `KEY` property today;
    /// no form UI yet (deferred to a dedicated PGP / X.509 issue).
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    /// Categories — already stored on Contact / ContactRow but
    /// the form couldn't edit them before #143.  Optional so
    /// callers that don't include the field leave the existing
    /// list intact via the merge logic in `update_contact`.
    #[serde(default)]
    pub categories: Option<Vec<String>>,
}

/// Create a new contact on Nextcloud and cache it locally.
///
/// `addressbook_url` is the absolute URL of the target book (the
/// `path` field on `Addressbook`). The UI picks it up from the
/// sync report or a dedicated listing command.
///
/// Generates a fresh UUID for the vCard's UID so callers don't
/// have to, and returns the newly cached `Contact` so the UI can
/// slot it straight into its list without re-fetching.
pub async fn create_contact(
    nc_id: String,
    addressbook_url: String,
    addressbook_name: String,
    input: ContactInput,
    cache: &Cache,
) -> Result<Contact, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;

    let uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let parsed = input_to_parsed(&uid, &input);
    let vcard = build_vcard(&parsed);

    let outcome = dav_create_contact_for(&account, &addressbook_url, &uid, &vcard).await?;

    let row = parsed_to_row(&outcome.href, &outcome.etag, &uid, &parsed, vcard);
    cache
        .upsert_single_contact(&nc_id, &addressbook_name, &row)
        .map_err(UnkaiError::from)?;

    Ok(row_to_contact(&nc_id, &addressbook_name, &row))
}

/// Replace an existing contact on the server with the form's new
/// values. `If-Match` on the cached etag means a concurrent edit
/// on another device surfaces as a 412 (mapped to a readable error)
/// rather than silently overwriting.
pub async fn update_contact(
    contact_id: String,
    input: ContactInput,
    cache: &Cache,
) -> Result<Contact, UnkaiError> {
    let handle = load_contact_handle(cache, &contact_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;

    // Merge the form fields over the existing parsed vCard so fields
    // the edit form doesn't surface (addresses, birthday, urls, note,
    // title, …) round-trip instead of being silently wiped on every
    // edit. The form-editable fields below replace whatever was there.
    let mut parsed = match unkai_carddav::parse_vcard(&handle.vcard_raw) {
        Ok(p) => p,
        Err(_) => ParsedVcard {
            uid: handle.vcard_uid.clone(),
            ..Default::default()
        },
    };
    parsed.uid = handle.vcard_uid.clone();
    parsed.display_name = input.display_name.clone();
    parsed.emails = input
        .emails
        .iter()
        .map(|e| unkai_carddav::VcardEmail {
            kind: e.kind.clone(),
            value: e.value.clone(),
        })
        .collect();
    parsed.phones = input
        .phones
        .iter()
        .map(|p| unkai_carddav::VcardPhone {
            kind: p.kind.clone(),
            value: p.value.clone(),
        })
        .collect();
    parsed.organization = input.organization.clone();
    if input.photo_data.is_some() {
        parsed.photo_mime = input.photo_mime.clone();
        parsed.photo_data = input.photo_data.clone();
    }
    // Extended fields: a UI that surfaces them sends the new value
    // (or `None` to clear); a UI that doesn't sends `Option::None`
    // for the *whole field*, in which case we leave the cached
    // value alone. The distinction is made via `serde(default)` on
    // `ContactInput` — `None` only ever appears when the JSON omits
    // the key entirely, never when the user explicitly cleared it.
    if let Some(t) = &input.title {
        parsed.title = if t.is_empty() { None } else { Some(t.clone()) };
    }
    if let Some(b) = &input.birthday {
        parsed.birthday = if b.is_empty() { None } else { Some(b.clone()) };
    }
    if let Some(n) = &input.note {
        parsed.note = if n.is_empty() { None } else { Some(n.clone()) };
    }
    if let Some(addrs) = &input.addresses {
        parsed.addresses = addrs
            .iter()
            .map(|a| unkai_carddav::VcardAddress {
                kind: a.kind.clone(),
                street: a.street.clone(),
                locality: a.locality.clone(),
                region: a.region.clone(),
                postal_code: a.postal_code.clone(),
                country: a.country.clone(),
            })
            .collect();
    }
    if let Some(urls) = &input.urls {
        parsed.urls = urls.clone();
    }
    // ── #143: vCard 4 fields ─────────────────────────────────
    // Same merge pattern as the older fields: a `Some` value
    // replaces what's cached (with empty-string clearing the
    // slot for scalar Options), `None` leaves the cached value
    // intact so a UI that doesn't surface the field can still
    // round-trip it.
    if let Some(sn) = &input.structured_name {
        parsed.structured_name = unkai_carddav::VcardStructuredName {
            family: sn.family.clone(),
            given: sn.given.clone(),
            additional: sn.additional.clone(),
            prefix: sn.prefix.clone(),
            suffix: sn.suffix.clone(),
        };
    }
    if let Some(n) = &input.nickname {
        parsed.nickname = if n.is_empty() { None } else { Some(n.clone()) };
    }
    if let Some(a) = &input.anniversary {
        parsed.anniversary = if a.is_empty() { None } else { Some(a.clone()) };
    }
    if let Some(g) = &input.gender {
        parsed.gender = if g.is_empty() { None } else { Some(g.clone()) };
    }
    if let Some(impp) = &input.impp {
        parsed.impp = impp
            .iter()
            .map(|i| unkai_carddav::VcardImpp {
                kind: i.kind.clone(),
                value: i.value.clone(),
            })
            .collect();
    }
    if let Some(r) = &input.role {
        parsed.role = if r.is_empty() { None } else { Some(r.clone()) };
    }
    if let Some(langs) = &input.languages {
        parsed.languages = langs.clone();
    }
    if let Some(g) = &input.geo {
        parsed.geo = if g.is_empty() { None } else { Some(g.clone()) };
    }
    if let Some(tz) = &input.timezone {
        parsed.timezone = if tz.is_empty() {
            None
        } else {
            Some(tz.clone())
        };
    }
    if let Some(ks) = &input.keys {
        parsed.keys = ks.clone();
    }
    if let Some(cats) = &input.categories {
        parsed.categories = cats.clone();
    }
    let vcard = build_vcard(&parsed);

    let outcome = dav_update_contact_for(&account, &handle.href, &handle.etag, &vcard).await?;

    let row = parsed_to_row(
        &outcome.href,
        &outcome.etag,
        &handle.vcard_uid,
        &parsed,
        vcard,
    );
    cache
        .upsert_single_contact(&handle.nextcloud_account_id, &handle.addressbook, &row)
        .map_err(UnkaiError::from)?;

    Ok(row_to_contact(
        &handle.nextcloud_account_id,
        &handle.addressbook,
        &row,
    ))
}

/// Delete a contact from the server and the local cache. The
/// server delete is gated on the cached etag; if that fails we
/// leave the cache row alone so the UI can show the user the
/// fresh state on the next sync.
pub async fn delete_contact(contact_id: String, cache: &Cache) -> Result<(), UnkaiError> {
    let handle = load_contact_handle(cache, &contact_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;

    dav_delete_contact_for(&account, &handle.href, &handle.etag).await?;

    cache
        .delete_contact_by_id(&contact_id)
        .map_err(UnkaiError::from)?;
    Ok(())
}

// ── Reserved Kontaktgruppe (#133 redesign) ────────────────────
//
// Manual mailing lists (KIND:group vCards) are auto-tagged with
// this CATEGORY so iOS / Apple Contacts / NC Contacts surface
// them in a dedicated "Mailing Lists" group.  The
// `list_mailing_lists` IPC filters this exact name out of the
// virtual-row derivation so we don't end up with a circular
// "Mailing Lists" mailing list of mailing lists.
pub const MAILING_LISTS_CATEGORY: &str = "Mailing Lists";

// ── Categories / Kontaktgruppen (#133 redesign) ──────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactCategoryView {
    /// CATEGORY name as written on the vCards.
    pub name: String,
    /// Number of cached contacts carrying this CATEGORY.
    pub member_count: u32,
    /// True when the user has flipped "Use as mailing list"
    /// off on this category — drives both "no virtual row in
    /// the Mailing Lists tab" and "no autocomplete suggestion".
    pub use_as_mailing_list: bool,
}

/// Distinct CATEGORIES across every cached contact, with the
/// per-row "use as mailing list" overlay applied.
///
/// First call after the v17 → v18 migration backfills the
/// `categories_json` column from the cached `vcard_raw` for
/// every row whose tag list is still empty.  Idempotent —
/// once a row has a non-empty `categories_json` it's skipped.
pub fn list_contact_categories(cache: &Cache) -> Result<Vec<ContactCategoryView>, UnkaiError> {
    let _ = cache.backfill_categories(|raw| {
        unkai_carddav::parse_vcard(raw)
            .map(|p| p.categories)
            .unwrap_or_default()
    });
    let cats = cache.list_contact_categories().map_err(UnkaiError::from)?;
    let suppressed = cache
        .get_mailing_list_suppressed()
        .map_err(UnkaiError::from)?;
    Ok(cats
        .into_iter()
        .filter(|(name, _)| name != MAILING_LISTS_CATEGORY)
        .map(|(name, member_count)| {
            let id = format!("cat:{name}");
            ContactCategoryView {
                use_as_mailing_list: !suppressed.contains(&id),
                name,
                member_count,
            }
        })
        .collect())
}

/// Toggle "use as mailing list" for one Kontaktgruppe.
pub fn set_category_use_as_mailing_list(
    name: String,
    enabled: bool,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let id = format!("cat:{name}");
    cache
        .set_mailing_list_suppressed(&id, !enabled)
        .map_err(UnkaiError::from)
}

/// Add a CATEGORIES tag to one contact's vCard, sync to the
/// server.  Idempotent — a contact already in the category is
/// left alone (no spurious PUT).
pub async fn add_contact_to_category(
    contact_id: String,
    category: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    rewrite_contact_categories(&contact_id, &cache, |cats| {
        if !cats.iter().any(|c| c == &category) {
            cats.push(category.clone());
            true
        } else {
            false
        }
    })
    .await
}

/// Remove one CATEGORIES tag from a contact's vCard.
pub async fn remove_contact_from_category(
    contact_id: String,
    category: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    rewrite_contact_categories(&contact_id, &cache, |cats| {
        let before = cats.len();
        cats.retain(|c| c != &category);
        cats.len() != before
    })
    .await
}

/// Rename a category across every contact carrying it.  Loops
/// each tagged contact, rewrites the CATEGORIES list, PUTs.
/// Best-effort per-contact: a failure on one row logs and
/// continues so a flaky network doesn't strand the rename
/// half-applied (the next sync would heal anyway).
pub async fn rename_contact_category(
    old: String,
    new: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let new = new.trim().to_string();
    if new.is_empty() {
        return Err(UnkaiError::Other("new category name is empty".into()));
    }
    let contacts = cache
        .list_contacts_with_category(&old)
        .map_err(UnkaiError::from)?;
    for c in contacts {
        if let Err(e) = rewrite_contact_categories_inner(&c.id, cache, |cats| {
            let mut changed = false;
            for cat in cats.iter_mut() {
                if cat == &old {
                    *cat = new.clone();
                    changed = true;
                }
            }
            if !cats.iter().any(|c| c == &new) {
                cats.push(new.clone());
                changed = true;
            }
            cats.retain(|c| c != &old);
            changed
        })
        .await
        {
            tracing::warn!("rename category on {}: {e}", c.id);
        }
    }
    // Carry the suppressed flag over to the new id so the
    // user's "use as mailing list" choice doesn't reset.
    let suppressed = cache
        .get_mailing_list_suppressed()
        .map_err(UnkaiError::from)?;
    if suppressed.contains(&format!("cat:{old}")) {
        cache
            .set_mailing_list_suppressed(&format!("cat:{old}"), false)
            .map_err(UnkaiError::from)?;
        cache
            .set_mailing_list_suppressed(&format!("cat:{new}"), true)
            .map_err(UnkaiError::from)?;
    }
    // Carry the per-list emoji overlay across the rename too.
    cache
        .rename_mailing_list_setting(&format!("cat:{old}"), &format!("cat:{new}"))
        .map_err(UnkaiError::from)?;
    Ok(())
}

/// Delete a category — strips the tag from every contact.  The
/// underlying contacts are untouched, just no longer tagged.
pub async fn delete_contact_category(name: String, cache: &Cache) -> Result<(), UnkaiError> {
    let contacts = cache
        .list_contacts_with_category(&name)
        .map_err(UnkaiError::from)?;
    for c in contacts {
        if let Err(e) = rewrite_contact_categories_inner(&c.id, cache, |cats| {
            let before = cats.len();
            cats.retain(|cc| cc != &name);
            cats.len() != before
        })
        .await
        {
            tracing::warn!("delete category on {}: {e}", c.id);
        }
    }
    Ok(())
}

/// Public wrapper that takes a `&Cache` and forwards
/// to the private inner — keeps the create/rename/delete IPCs
/// tidy without making them all duplicate the cache extraction.
pub async fn rewrite_contact_categories<F>(
    contact_id: &str,
    cache: &&Cache,
    f: F,
) -> Result<(), UnkaiError>
where
    F: FnOnce(&mut Vec<String>) -> bool,
{
    rewrite_contact_categories_inner(contact_id, cache, f).await
}

/// Pull the cached vCard for `contact_id`, mutate its
/// CATEGORIES list via `f`, and PUT the rewritten body back to
/// CardDAV.  Returns early when `f` reports no change so we
/// don't burn a round-trip on a no-op.
pub async fn rewrite_contact_categories_inner<F>(
    contact_id: &str,
    cache: &Cache,
    f: F,
) -> Result<(), UnkaiError>
where
    F: FnOnce(&mut Vec<String>) -> bool,
{
    let handle = load_contact_handle(cache, contact_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    let mut parsed = match unkai_carddav::parse_vcard(&handle.vcard_raw) {
        Ok(p) => p,
        Err(_) => ParsedVcard {
            uid: handle.vcard_uid.clone(),
            ..Default::default()
        },
    };
    parsed.uid = handle.vcard_uid.clone();
    let changed = f(&mut parsed.categories);
    if !changed {
        return Ok(());
    }
    let vcard = build_vcard(&parsed);
    let outcome = dav_update_contact_for(&account, &handle.href, &handle.etag, &vcard).await?;
    let row = parsed_to_row(
        &outcome.href,
        &outcome.etag,
        &handle.vcard_uid,
        &parsed,
        vcard,
    );
    cache
        .upsert_single_contact(&handle.nextcloud_account_id, &handle.addressbook, &row)
        .map_err(UnkaiError::from)?;
    Ok(())
}

// ── Unified mailing lists (#133 redesign) ─────────────────────
//
// Single IPC the Mailing Lists tab + AddressAutocomplete read
// from.  Combines four sources into one flat list:
//   * `cat:<name>`  — a Kontaktgruppe (CATEGORY tag) with
//     `use_as_mailing_list = true`.
//   * `group:<id>`  — an OCS user group.
//   * `team:<id>`   — a Circles / Teams entry.
//   * `list:<uid>`  — a manual KIND:group vCard.
// The reserved `Mailing Lists` category is filtered out so the
// auto-tag we put on every manual list doesn't generate a
// circular row.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailingListView {
    /// Unified id — see source-prefix list above.
    pub id: String,
    /// `category` | `nc-group` | `team` | `manual`.  Drives the
    /// pill colour + the CRUD affordances.
    pub source: String,
    pub name: String,
    pub members: Vec<MailingListMemberView>,
    /// Local-only flag — when true the row is suppressed from
    /// AddressAutocomplete.  Categories use the same flag for
    /// the "Use as mailing list" toggle (off → suppressed).
    pub hidden_from_autocomplete: bool,
    /// Local-only emoji avatar override; `None` falls back to
    /// the source's default icon (🏷️/📨/⚡).
    pub emoji: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailingListMemberView {
    pub display_name: String,
    pub email: String,
}

/// Build the unified mailing-list view across every source.
/// Read-heavy but cheap — categories are aggregated in one
/// SQL pass and the NC group / team list reuses the existing
/// list_nextcloud_groups path.
pub async fn list_mailing_lists(cache: &Cache) -> Result<Vec<MailingListView>, UnkaiError> {
    // Same lazy backfill list_contact_categories does — this
    // IPC is the entry point the autocomplete uses on first
    // launch, before the contacts UI was opened, so we have to
    // rehydrate categories here too or the category-derived
    // rows would surface with zero members.
    let _ = cache.backfill_categories(|raw| {
        unkai_carddav::parse_vcard(raw)
            .map(|p| p.categories)
            .unwrap_or_default()
    });
    let suppressed = cache
        .get_mailing_list_suppressed()
        .map_err(UnkaiError::from)?;
    let emojis = cache.get_mailing_list_emojis().map_err(UnkaiError::from)?;
    let mut out: Vec<MailingListView> = Vec::new();

    // 1. Categories.  Skip the reserved one we use as a holder
    // for KIND:group vCards.
    let cats = cache.list_contact_categories().map_err(UnkaiError::from)?;
    for (name, _count) in cats {
        if name == MAILING_LISTS_CATEGORY {
            continue;
        }
        let id = format!("cat:{name}");
        // Category rows stay in the Lists tab regardless of
        // the hide flag, so the user can toggle them back on
        // from the same swatch they used to turn them off.
        // The autocomplete client-side filter is what actually
        // suppresses suggestions; the row carries the flag so
        // the UI can render it greyed-out.
        let hidden_from_autocomplete = suppressed.contains(&id);
        let contacts = cache.list_contacts_with_category(&name).unwrap_or_default();
        // Drop members that have no email — a category-derived
        // mailing list is only useful as a sending target, and
        // a row with empty email would just be noise (and
        // would expand to an unaddressable entry in compose
        // autocomplete).  Contacts without email still show
        // up in the Contacts tab under their Contact Group;
        // they only get hidden here in the mailing-list view.
        let members: Vec<MailingListMemberView> = contacts
            .into_iter()
            .filter_map(|c| {
                let email = c
                    .email
                    .into_iter()
                    .next()
                    .map(|e| e.value)
                    .unwrap_or_default();
                if email.is_empty() {
                    None
                } else {
                    Some(MailingListMemberView {
                        display_name: c.display_name,
                        email,
                    })
                }
            })
            .collect();
        let emoji = emojis.get(&id).cloned();
        out.push(MailingListView {
            id,
            source: "category".to_string(),
            name,
            members,
            hidden_from_autocomplete,
            emoji,
        });
    }

    // 2. Manual KIND:group vCards.  These already auto-tag the
    // reserved category so they show up in the Mailing Lists
    // Kontaktgruppe in NC; here we render them directly.
    if let Ok(groups) = cache.list_contact_groups() {
        for g in groups {
            let id = format!("list:{}", g.id);
            let suppressed_row = suppressed.contains(&id);
            let resolved = cache
                .resolve_group_members(&g.nextcloud_account_id, &g.member_uids)
                .unwrap_or_default();
            let members = resolved
                .into_iter()
                .map(|(_id, name, email)| MailingListMemberView {
                    display_name: name,
                    email,
                })
                .collect();
            let emoji = emojis.get(&id).cloned().or_else(|| g.emoji.clone());
            out.push(MailingListView {
                id,
                source: "manual".to_string(),
                name: g.display_name,
                members,
                hidden_from_autocomplete: suppressed_row,
                emoji,
            });
        }
    }

    // 3. Teams.  list_nextcloud_groups already returns OCS
    // user groups + Circles unified under `source = "team"`
    // with cleaned display names — we just forward each row
    // verbatim.  These refresh every call (typically a handful
    // per server, so live OCS round-trip is fine).
    let nc_groups = list_nextcloud_groups(cache).await.unwrap_or_default();
    for g in nc_groups {
        let id = g.id;
        let suppressed_row = suppressed.contains(&id);
        let members = g
            .members
            .into_iter()
            .map(|m| MailingListMemberView {
                display_name: m.display_name,
                email: m.email,
            })
            .collect();
        let emoji = emojis.get(&id).cloned();
        out.push(MailingListView {
            id,
            source: "team".to_string(),
            name: g.display_name,
            members,
            hidden_from_autocomplete: suppressed_row,
            emoji,
        });
    }

    Ok(out)
}

/// Toggle the local hide-from-autocomplete flag for one
/// mailing-list row.  Used by the per-row swatch on
/// non-category rows (manual / NC group / team) — categories
/// use `set_category_use_as_mailing_list` which writes to the
/// same table under the `cat:` id space.
pub fn set_mailing_list_hidden(id: String, hidden: bool, cache: &Cache) -> Result<(), UnkaiError> {
    cache
        .set_mailing_list_suppressed(&id, hidden)
        .map_err(UnkaiError::from)
}

/// Set (or clear) the per-list emoji avatar override.  An empty
/// string clears the override so the row falls back to its
/// source icon.  Works for category / manual / team rows alike,
/// keyed by the unified id.
pub fn set_mailing_list_emoji(
    id: String,
    emoji: Option<String>,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    cache
        .set_mailing_list_emoji(&id, emoji.as_deref().filter(|s| !s.is_empty()))
        .map_err(UnkaiError::from)
}

/// Rename a mailing list, dispatched on the unified id prefix.
/// `cat:<name>` rewrites the CATEGORIES tag on every member
/// contact; `list:<uid>` updates the KIND:group vCard's
/// `display_name`.  Teams are read-only and rejected.
pub async fn rename_mailing_list(
    id: String,
    new_name: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let new_name = new_name.trim().to_string();
    if new_name.is_empty() {
        return Err(UnkaiError::Other("new name is empty".into()));
    }
    if let Some(old) = id.strip_prefix("cat:") {
        rename_contact_category(old.to_string(), new_name, cache).await
    } else if let Some(group_id) = id.strip_prefix("list:") {
        // Reuse update_contact_group with the existing member
        // list — passing None for member_uids keeps them intact.
        update_contact_group(group_id.to_string(), Some(new_name), None, cache)
            .await
            .map(|_| ())
    } else {
        Err(UnkaiError::Other("teams cannot be renamed".into()))
    }
}

// ── Contact groups / mailing lists (#133, #113) ───────────────
//
// Groups are stored on the server as plain `KIND:group` vCards.
// The CardDAV layer doesn't care — they sync just like
// individuals — so the IPCs here are thin wrappers that build the
// right vCard shape, route writes through the same
// create/update/delete CardDAV path the contacts use, and surface
// the local-only `group_emoji` / `group_hidden` overlay from the
// cache.

/// Snapshot of a group, hydrated for the UI.  `members` is the
/// expanded list of contact rows so the picker / chip strip can
/// render names + first emails without a second round-trip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactGroupView {
    pub id: String,
    pub nextcloud_account_id: String,
    pub display_name: String,
    pub member_uids: Vec<String>,
    pub members: Vec<GroupMemberView>,
    pub emoji: Option<String>,
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMemberView {
    /// Composite contact id (`{nc}::{uid}`) — matches what
    /// `get_contacts` / `search_contacts` already expose.
    pub id: String,
    pub display_name: String,
    /// First email address, or empty when the underlying vCard
    /// has none — the UI shows "no email" in that case rather
    /// than failing the expand.
    pub email: String,
}

/// List every contact group across every connected NC account,
/// each with its members already resolved to (id, name, email)
/// triples so the UI doesn't have to chase referenced UIDs.
pub fn list_contact_groups(cache: &Cache) -> Result<Vec<ContactGroupView>, UnkaiError> {
    let groups = cache.list_contact_groups().map_err(UnkaiError::from)?;
    let mut out = Vec::with_capacity(groups.len());
    for g in groups {
        let resolved = cache
            .resolve_group_members(&g.nextcloud_account_id, &g.member_uids)
            .map_err(UnkaiError::from)?;
        let members = resolved
            .into_iter()
            .map(|(id, display_name, email)| GroupMemberView {
                id,
                display_name,
                email,
            })
            .collect();
        out.push(ContactGroupView {
            id: g.id,
            nextcloud_account_id: g.nextcloud_account_id,
            display_name: g.display_name,
            member_uids: g.member_uids,
            members,
            emoji: g.emoji,
            hidden: g.hidden,
        });
    }
    Ok(out)
}

/// Create a new `KIND:group` vCard on the server and cache it.
/// `member_uids` is the bare-UID list (no `urn:uuid:` prefix);
/// the writer wraps each in the canonical URI form.
pub async fn create_contact_group(
    nc_id: String,
    addressbook_url: String,
    addressbook_name: String,
    display_name: String,
    member_uids: Vec<String>,
    cache: &Cache,
) -> Result<ContactGroupView, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;

    let uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let parsed = ParsedVcard {
        uid: uid.clone(),
        display_name: display_name.clone(),
        kind: "group".to_string(),
        members: member_uids
            .iter()
            .map(|u| {
                if u.starts_with("urn:uuid:") {
                    u.clone()
                } else {
                    format!("urn:uuid:{u}")
                }
            })
            .collect(),
        // Auto-tag manual mailing lists with the reserved
        // CATEGORY so iOS / NC Contacts surface them in a
        // dedicated Kontaktgruppe.  The list_mailing_lists IPC
        // filters this name out of the virtual-row derivation
        // so we don't end up with a circular "Mailing Lists"
        // mailing list of mailing lists.
        categories: vec![MAILING_LISTS_CATEGORY.to_string()],
        ..Default::default()
    };
    let vcard = build_vcard(&parsed);
    let outcome = dav_create_contact_for(&account, &addressbook_url, &uid, &vcard).await?;
    let row = parsed_to_row(&outcome.href, &outcome.etag, &uid, &parsed, vcard);
    cache
        .upsert_single_contact(&nc_id, &addressbook_name, &row)
        .map_err(UnkaiError::from)?;
    let id = format!("{nc_id}::{uid}");
    Ok(ContactGroupView {
        id,
        nextcloud_account_id: nc_id,
        display_name,
        member_uids,
        members: Vec::new(),
        emoji: None,
        hidden: false,
    })
}

/// Edit an existing group — rename, swap members, both, neither.
/// `display_name` and `member_uids` are optional to keep the IPC
/// usable for partial updates from drag-and-drop (members only)
/// versus the rename modal (name only).
pub async fn update_contact_group(
    group_id: String,
    display_name: Option<String>,
    member_uids: Option<Vec<String>>,
    cache: &Cache,
) -> Result<ContactGroupView, UnkaiError> {
    let handle = load_contact_handle(cache, &group_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;

    let mut parsed = match unkai_carddav::parse_vcard(&handle.vcard_raw) {
        Ok(p) => p,
        Err(_) => ParsedVcard {
            uid: handle.vcard_uid.clone(),
            ..Default::default()
        },
    };
    parsed.uid = handle.vcard_uid.clone();
    parsed.kind = "group".to_string();
    if let Some(n) = display_name {
        parsed.display_name = n;
    }
    if let Some(uids) = member_uids {
        parsed.members = uids
            .iter()
            .map(|u| {
                if u.starts_with("urn:uuid:") {
                    u.clone()
                } else {
                    format!("urn:uuid:{u}")
                }
            })
            .collect();
    }
    let vcard = build_vcard(&parsed);
    let outcome = dav_update_contact_for(&account, &handle.href, &handle.etag, &vcard).await?;
    let row = parsed_to_row(
        &outcome.href,
        &outcome.etag,
        &handle.vcard_uid,
        &parsed,
        vcard,
    );
    cache
        .upsert_single_contact(&handle.nextcloud_account_id, &handle.addressbook, &row)
        .map_err(UnkaiError::from)?;
    // Re-pull the group with members hydrated so callers can
    // refresh their UI from a single response.
    let groups = cache.list_contact_groups().map_err(UnkaiError::from)?;
    let g = groups
        .into_iter()
        .find(|g| g.id == group_id)
        .ok_or_else(|| UnkaiError::Other(format!("group '{group_id}' missing after update")))?;
    let resolved = cache
        .resolve_group_members(&g.nextcloud_account_id, &g.member_uids)
        .map_err(UnkaiError::from)?;
    Ok(ContactGroupView {
        id: g.id,
        nextcloud_account_id: g.nextcloud_account_id,
        display_name: g.display_name,
        member_uids: g.member_uids,
        members: resolved
            .into_iter()
            .map(|(id, display_name, email)| GroupMemberView {
                id,
                display_name,
                email,
            })
            .collect(),
        emoji: g.emoji,
        hidden: g.hidden,
    })
}

/// Delete a contact group from the server + local cache.
pub async fn delete_contact_group(group_id: String, cache: &Cache) -> Result<(), UnkaiError> {
    let handle = load_contact_handle(cache, &group_id)?;
    let account = load_nextcloud_account(&handle.nextcloud_account_id)?;
    dav_delete_contact_for(&account, &handle.href, &handle.etag).await?;
    cache
        .delete_contact_by_id(&group_id)
        .map_err(UnkaiError::from)?;
    Ok(())
}

/// Local-only "hide this group" toggle — drives the contacts
/// sidebar's hidden state and excludes the group from the
/// AddressAutocomplete search.
pub fn set_contact_group_hidden(
    group_id: String,
    hidden: bool,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    cache
        .set_contact_group_hidden(&group_id, hidden)
        .map_err(UnkaiError::from)
}

/// Local-only emoji avatar overlay for a group.  `None` clears
/// it back to the initials fallback.
pub fn set_contact_group_emoji(
    group_id: String,
    emoji: Option<String>,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let val = emoji.as_deref().filter(|s| !s.is_empty());
    cache
        .set_contact_group_emoji(&group_id, val)
        .map_err(UnkaiError::from)
}

/// A trimmed-down addressbook record for the UI's "save new contact
/// to…" dropdown. We don't ship ctags or sync tokens — those are
/// sync-layer bookkeeping the frontend has no business touching.
#[derive(Debug, Clone, Serialize)]
pub struct AddressbookSummary {
    pub path: String,
    pub name: String,
    pub display_name: Option<String>,
}

/// List the user's addressbooks on a Nextcloud account. Used by
/// the Contacts view to populate a target-addressbook dropdown
/// when creating a new contact. Hits the server (PROPFIND) because
/// the list can change between logins and we want a fresh view.
pub async fn list_nextcloud_addressbooks(
    nc_id: String,
) -> Result<Vec<AddressbookSummary>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    // A local source has exactly the one addressbook seeded at add
    // time (#413) — nothing to probe.
    if account.is_local() {
        return Ok(vec![AddressbookSummary {
            path: format!("local://{nc_id}/{LOCAL_ADDRESSBOOK_NAME}"),
            name: LOCAL_ADDRESSBOOK_NAME.to_string(),
            display_name: Some("Contacts".to_string()),
        }]);
    }
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let books: Vec<Addressbook> = list_addressbooks_at(
        &carddav_home_of(&account),
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    Ok(books
        .into_iter()
        .map(|b| AddressbookSummary {
            path: b.path,
            name: b.name,
            display_name: b.display_name,
        })
        .collect())
}

/// Fold a `ContactInput` into the shape `build_vcard` expects. The
/// UID is pulled from the caller because the two code paths (create
/// vs. update) source it differently — a fresh UUID vs. the cached
/// one.
pub fn input_to_parsed(uid: &str, input: &ContactInput) -> ParsedVcard {
    // Auto-derive FN from the structured-name parts when the
    // user filled them in but left `display_name` blank — same
    // convention every desktop contacts app uses (RFC 6350 §6.2.1
    // requires FN, but the form lets users type only the broken-
    // out pieces).  When both are present, `display_name` from
    // the form wins so an explicit override is honoured.
    let derived_fn = input
        .structured_name
        .as_ref()
        .map(|n| {
            [
                n.prefix.trim(),
                n.given.trim(),
                n.additional.trim(),
                n.family.trim(),
                n.suffix.trim(),
            ]
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<&str>>()
            .join(" ")
        })
        .unwrap_or_default();
    let fn_value = if !input.display_name.trim().is_empty() {
        input.display_name.clone()
    } else if !derived_fn.is_empty() {
        derived_fn
    } else {
        String::new()
    };
    ParsedVcard {
        uid: uid.to_string(),
        display_name: fn_value,
        emails: input
            .emails
            .iter()
            .map(|e| unkai_carddav::VcardEmail {
                kind: e.kind.clone(),
                value: e.value.clone(),
            })
            .collect(),
        phones: input
            .phones
            .iter()
            .map(|p| unkai_carddav::VcardPhone {
                kind: p.kind.clone(),
                value: p.value.clone(),
            })
            .collect(),
        organization: input.organization.clone(),
        photo_mime: input.photo_mime.clone(),
        photo_data: input.photo_data.clone(),
        title: input.title.clone(),
        birthday: input.birthday.clone(),
        note: input.note.clone(),
        addresses: input
            .addresses
            .as_ref()
            .map(|list| {
                list.iter()
                    .map(|a| unkai_carddav::VcardAddress {
                        kind: a.kind.clone(),
                        street: a.street.clone(),
                        locality: a.locality.clone(),
                        region: a.region.clone(),
                        postal_code: a.postal_code.clone(),
                        country: a.country.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        urls: input.urls.clone().unwrap_or_default(),
        kind: String::new(),
        members: Vec::new(),
        categories: input.categories.clone().unwrap_or_default(),
        // ── #143 ─────────────────────────────────────────────
        structured_name: input
            .structured_name
            .as_ref()
            .map(|n| unkai_carddav::VcardStructuredName {
                family: n.family.clone(),
                given: n.given.clone(),
                additional: n.additional.clone(),
                prefix: n.prefix.clone(),
                suffix: n.suffix.clone(),
            })
            .unwrap_or_default(),
        nickname: input.nickname.clone(),
        anniversary: input.anniversary.clone(),
        gender: input.gender.clone(),
        impp: input
            .impp
            .as_ref()
            .map(|list| {
                list.iter()
                    .map(|i| unkai_carddav::VcardImpp {
                        kind: i.kind.clone(),
                        value: i.value.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        role: input.role.clone(),
        languages: input.languages.clone().unwrap_or_default(),
        geo: input.geo.clone(),
        timezone: input.timezone.clone(),
        keys: input.keys.clone().unwrap_or_default(),
    }
}

/// Build a `ContactRow` from a freshly-PUT vCard's outcome. Extracted
/// so create/update both ship the same set of extended fields
/// (addresses, birthday, urls, note, title) into the cache.
pub fn parsed_to_row(
    href: &str,
    etag: &str,
    uid: &str,
    parsed: &ParsedVcard,
    vcard_raw: String,
) -> ContactRow {
    ContactRow {
        href: href.to_string(),
        etag: etag.to_string(),
        vcard_uid: uid.to_string(),
        display_name: parsed.display_name.clone(),
        emails: parsed
            .emails
            .iter()
            .map(|e| unkai_core::models::ContactEmail {
                kind: e.kind.clone(),
                value: e.value.clone(),
            })
            .collect(),
        phones: parsed
            .phones
            .iter()
            .map(|p| unkai_core::models::ContactPhone {
                kind: p.kind.clone(),
                value: p.value.clone(),
            })
            .collect(),
        organization: parsed.organization.clone(),
        photo_mime: parsed.photo_mime.clone(),
        photo_data: parsed.photo_data.clone(),
        title: parsed.title.clone(),
        birthday: parsed.birthday.clone(),
        note: parsed.note.clone(),
        addresses: parsed
            .addresses
            .iter()
            .map(|a| unkai_core::models::ContactAddress {
                kind: a.kind.clone(),
                street: a.street.clone(),
                locality: a.locality.clone(),
                region: a.region.clone(),
                postal_code: a.postal_code.clone(),
                country: a.country.clone(),
            })
            .collect(),
        urls: parsed.urls.clone(),
        vcard_raw,
        kind: parsed.kind.clone(),
        member_uids: parsed.members.clone(),
        categories: parsed.categories.clone(),
    }
}

/// Hydrate a freshly-written `ContactRow` into a UI-facing
/// `Contact`. The composite id has to match the one the store
/// uses internally (`{nc_account_id}::{vcard_uid}`) so the next
/// `get_contacts` call returns the same record.
pub fn row_to_contact(nc_account_id: &str, addressbook: &str, row: &ContactRow) -> Contact {
    // #143: re-parse `vcard_raw` to recover the extended vCard 4
    // fields the cache schema doesn't store as dedicated columns
    // (structured-name parts, nickname, anniversary, gender, impp,
    // role, languages, geo, timezone, keys).  Round-tripping
    // through the cached body avoids a schema migration; cost is
    // one parse per contact returned to the UI, which is
    // negligible (the parser is microseconds for a typical
    // vCard).  When parsing fails — corrupt cached body, malformed
    // server data, etc. — we fall back to defaults so the rest of
    // the contact still renders.
    let extra = unkai_carddav::parse_vcard(&row.vcard_raw).ok();
    let structured_name = extra
        .as_ref()
        .map(|p| unkai_core::models::StructuredName {
            family: p.structured_name.family.clone(),
            given: p.structured_name.given.clone(),
            additional: p.structured_name.additional.clone(),
            prefix: p.structured_name.prefix.clone(),
            suffix: p.structured_name.suffix.clone(),
        })
        .unwrap_or_default();
    let nickname = extra.as_ref().and_then(|p| p.nickname.clone());
    let anniversary = extra.as_ref().and_then(|p| p.anniversary.clone());
    let gender = extra.as_ref().and_then(|p| p.gender.clone());
    let impp: Vec<unkai_core::models::ContactImpp> = extra
        .as_ref()
        .map(|p| {
            p.impp
                .iter()
                .map(|i| unkai_core::models::ContactImpp {
                    kind: i.kind.clone(),
                    value: i.value.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let role = extra.as_ref().and_then(|p| p.role.clone());
    let languages = extra
        .as_ref()
        .map(|p| p.languages.clone())
        .unwrap_or_default();
    let geo = extra.as_ref().and_then(|p| p.geo.clone());
    let timezone = extra.as_ref().and_then(|p| p.timezone.clone());
    let keys = extra.as_ref().map(|p| p.keys.clone()).unwrap_or_default();
    Contact {
        id: format!("{nc_account_id}::{}", row.vcard_uid),
        nextcloud_account_id: nc_account_id.to_string(),
        addressbook: addressbook.to_string(),
        display_name: row.display_name.clone(),
        email: row.emails.clone(),
        phone: row.phones.clone(),
        organization: row.organization.clone(),
        photo_mime: row.photo_mime.clone(),
        photo_data: row.photo_data.clone(),
        title: row.title.clone(),
        birthday: row.birthday.clone(),
        note: row.note.clone(),
        addresses: row.addresses.clone(),
        urls: row.urls.clone(),
        kind: row.kind.clone(),
        categories: row.categories.clone(),
        structured_name,
        nickname,
        anniversary,
        gender,
        impp,
        role,
        languages,
        geo,
        timezone,
        keys,
    }
}

pub fn load_contact_handle(
    cache: &Cache,
    contact_id: &str,
) -> Result<ContactServerHandle, UnkaiError> {
    cache
        .get_contact_server_handle(contact_id)
        .map_err(UnkaiError::from)?
        .ok_or_else(|| {
            UnkaiError::Other(format!(
                "contact '{contact_id}' is not in the local cache — refresh and try again"
            ))
        })
}

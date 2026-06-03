//! S/MIME (X.509) certificate cache (#338).
//!
//! The X.509 counterpart to [`crate::cache::pgp_keys`].  Stores the
//! leaf certificates of *recipients* (and signers of inbound mail) so
//! the Compose layer can answer "do we have a cert for bob@example.com?"
//! and the receive layer can answer "is this signature from a cert we
//! know?" without re-parsing vCards or asking the user.
//!
//! Each row carries the DER-encoded certificate blob plus a `source`
//! string telling us where it came from (`'vcard'` for certs
//! auto-imported from a Nextcloud contact's `KEY:` property, `'manual'`
//! for paste-in-the-cert flows, `'inbound-message'` for certs learned
//! from a signed inbound mail).  Provenance isn't security-load-bearing
//! — the trust model is "cert fingerprint, period" — but the settings
//! panel surfaces it so the user can audit how each cert got into their
//! cache.
//!
//! The user's *own* certificate + private key live in the OS keychain
//! as a PKCS#12 bundle, not here — see
//! `credentials::store_smime_private_cert`.
//!
//! ## Why a sibling table, not an extension of `pgp_public_keys`
//!
//! The two formats share no storage worth unifying: OpenPGP keys are an
//! armored ASCII block (`armored_key TEXT`), X.509 certs are binary DER
//! (`der_cert BLOB`).  Keeping them in separate tables means a change to
//! one format's storage can't churn the other's, and the `source` /
//! `email` / `fingerprint` columns line up so callers can treat the two
//! caches through parallel APIs.

use rusqlite::{OptionalExtension, params};
use tracing::info;

use crate::cache::{Cache, CacheError};

/// Tag describing where a cached certificate came from.  Stored in the
/// `source` column as the lowercase kebab-case `as_str()` value.
///
/// Deliberately the same three-variant shape as
/// [`crate::cache::pgp_keys::PgpKeySource`] so the two caches stay
/// parallel — the wire-stack distinction lives in the protocol code,
/// not the provenance enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmimeCertSource {
    /// Auto-imported from a Nextcloud contact's vCard `KEY:` property.
    Vcard,
    /// Pasted in by the user via the Compose / settings UI.
    Manual,
    /// Learned from an inbound signed message.
    InboundMessage,
}

impl SmimeCertSource {
    /// Serialised form used in the `smime_certs.source` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vcard => "vcard",
            Self::Manual => "manual",
            Self::InboundMessage => "inbound-message",
        }
    }

    /// Inverse of [`Self::as_str`].  Unknown values map to `Manual` as
    /// a conservative fallback — the row exists, so somebody put it
    /// there deliberately; treating it as "manual" preserves the row
    /// without claiming a stronger provenance than we can prove.
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "vcard" => Self::Vcard,
            "inbound-message" => Self::InboundMessage,
            _ => Self::Manual,
        }
    }
}

/// One row in the `smime_certs` table.  Public DTO; the SQL helpers
/// take and return this shape so callers don't have to thread five
/// arguments through every call.
#[derive(Debug, Clone)]
pub struct SmimeCertRow {
    /// SHA-256 fingerprint of the DER-encoded certificate — uppercase
    /// hex with colons, the form `openssl x509 -fingerprint -sha256`
    /// emits.  Primary key in the table; uniqueness is the only
    /// invariant we enforce at the SQL layer.
    pub fingerprint: String,
    /// Binding email address from the certificate's Subject Alternative
    /// Name (`rfc822Name`), when present — RFC 8551 §3 mandates the
    /// email live in SAN, not CN.  `None` when the cert carries no SAN
    /// email.  Indexed for recipient lookups.
    pub email: Option<String>,
    /// Canonical DER bytes of the leaf certificate, exactly as parsed.
    /// We keep the binary wire form (not PEM) so round-tripping through
    /// the database can't drift the fingerprint.
    pub der_cert: Vec<u8>,
    /// Where this cert came from — see [`SmimeCertSource`].
    pub source: SmimeCertSource,
    /// Unix epoch seconds when the row landed.  Used by the settings
    /// panel to show "added 3 days ago".
    pub added_at: i64,
}

impl Cache {
    /// Insert (or replace by fingerprint) one certificate.  Replace
    /// semantics let the auto-import path safely re-run on every
    /// contact sync without duplicate-key errors — the fingerprint is
    /// the canonical identity, so a re-arriving identical cert is just
    /// a no-op in user-facing terms.
    pub fn upsert_smime_cert(&self, row: &SmimeCertRow) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO smime_certs
                (fingerprint, email, der_cert, source, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (fingerprint) DO UPDATE SET
                email     = excluded.email,
                der_cert  = excluded.der_cert,
                source    = excluded.source,
                added_at  = excluded.added_at",
            params![
                row.fingerprint,
                row.email,
                row.der_cert,
                row.source.as_str(),
                row.added_at,
            ],
        )?;
        info!(
            "Cached S/MIME cert fp={} email={} source={}",
            row.fingerprint,
            row.email.as_deref().unwrap_or("<none>"),
            row.source.as_str(),
        );
        Ok(())
    }

    /// Look up a cached certificate by its fingerprint.  Returns `None`
    /// when we don't have a cert with that fingerprint — the caller
    /// then decides whether to prompt the user to paste one in.
    pub fn get_smime_cert_by_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<Option<SmimeCertRow>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT fingerprint, email, der_cert, source, added_at
                 FROM smime_certs
                 WHERE fingerprint = ?1",
                params![fingerprint],
                row_from_columns,
            )
            .optional()?;
        Ok(row)
    }

    /// Look up every cached certificate claiming a given email address.
    /// Returns a vec because a contact can hold more than one cert
    /// (renewal / rotation overlap) — the Compose layer should
    /// typically pick the most recently added (the order this query
    /// returns).
    pub fn get_smime_certs_for_email(&self, email: &str) -> Result<Vec<SmimeCertRow>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT fingerprint, email, der_cert, source, added_at
             FROM smime_certs
             WHERE email = ?1
             ORDER BY added_at DESC",
        )?;
        let rows = stmt.query_map(params![email], row_from_columns)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Enumerate every cached certificate, newest first.  Used by the
    /// settings panel to render the "Known recipient certificates" list.
    pub fn list_smime_certs(&self) -> Result<Vec<SmimeCertRow>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT fingerprint, email, der_cert, source, added_at
             FROM smime_certs
             ORDER BY added_at DESC",
        )?;
        let rows = stmt.query_map([], row_from_columns)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Remove one cached certificate by fingerprint.  Silently succeeds
    /// when the fingerprint isn't in the table — matches the keychain
    /// helpers' "no-op on missing" contract so the UI can fire-and-forget.
    pub fn delete_smime_cert(&self, fingerprint: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let removed = conn.execute(
            "DELETE FROM smime_certs WHERE fingerprint = ?1",
            params![fingerprint],
        )?;
        if removed > 0 {
            info!("Deleted S/MIME cert fp={fingerprint}");
        }
        Ok(())
    }
}

fn row_from_columns(r: &rusqlite::Row<'_>) -> rusqlite::Result<SmimeCertRow> {
    let source: String = r.get(3)?;
    Ok(SmimeCertRow {
        fingerprint: r.get(0)?,
        email: r.get(1)?,
        der_cert: r.get(2)?,
        source: SmimeCertSource::from_db_str(&source),
        added_at: r.get(4)?,
    })
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Cache {
        Cache::open_in_memory().expect("open in-memory cache")
    }

    fn sample_row(fp: &str, email: Option<&str>, source: SmimeCertSource) -> SmimeCertRow {
        SmimeCertRow {
            fingerprint: fp.to_string(),
            email: email.map(|s| s.to_string()),
            // Stand-in DER bytes; the cache layer treats the blob as
            // opaque, so a real X.509 encoding isn't needed to exercise
            // round-trip / lookup behaviour.
            der_cert: format!("DER-{fp}").into_bytes(),
            source,
            added_at: 1_700_000_000,
        }
    }

    #[test]
    fn upsert_and_lookup_by_fingerprint() {
        let cache = open();
        let row = sample_row(
            "AB:CD:11",
            Some("alice@example.com"),
            SmimeCertSource::Vcard,
        );
        cache.upsert_smime_cert(&row).unwrap();

        let got = cache
            .get_smime_cert_by_fingerprint("AB:CD:11")
            .unwrap()
            .expect("row should exist");
        assert_eq!(got.fingerprint, "AB:CD:11");
        assert_eq!(got.email.as_deref(), Some("alice@example.com"));
        assert_eq!(got.der_cert, b"DER-AB:CD:11");
        assert_eq!(got.source, SmimeCertSource::Vcard);

        assert!(
            cache
                .get_smime_cert_by_fingerprint("missing")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn lookup_by_email_returns_newest_first() {
        let cache = open();
        cache
            .upsert_smime_cert(&SmimeCertRow {
                added_at: 1_700_000_000,
                ..sample_row("AA:01", Some("alice@example.com"), SmimeCertSource::Vcard)
            })
            .unwrap();
        cache
            .upsert_smime_cert(&SmimeCertRow {
                added_at: 1_800_000_000, // newer
                ..sample_row("BB:02", Some("alice@example.com"), SmimeCertSource::Manual)
            })
            .unwrap();

        let got = cache
            .get_smime_certs_for_email("alice@example.com")
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].fingerprint, "BB:02", "newest first");
        assert_eq!(got[1].fingerprint, "AA:01");
    }

    #[test]
    fn upsert_replaces_existing_row_for_same_fingerprint() {
        let cache = open();
        cache
            .upsert_smime_cert(&sample_row(
                "CC:03",
                Some("old@example.com"),
                SmimeCertSource::Vcard,
            ))
            .unwrap();
        cache
            .upsert_smime_cert(&sample_row(
                "CC:03",
                Some("new@example.com"),
                SmimeCertSource::Manual,
            ))
            .unwrap();

        let got = cache
            .get_smime_cert_by_fingerprint("CC:03")
            .unwrap()
            .unwrap();
        assert_eq!(got.email.as_deref(), Some("new@example.com"));
        assert_eq!(got.source, SmimeCertSource::Manual);

        // And we end up with one row, not two.
        let all = cache.list_smime_certs().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn delete_is_noop_on_missing_fingerprint() {
        let cache = open();
        cache.delete_smime_cert("does-not-exist").unwrap();
    }

    #[test]
    fn source_round_trip_via_string() {
        assert_eq!(SmimeCertSource::Vcard.as_str(), "vcard");
        assert_eq!(SmimeCertSource::Manual.as_str(), "manual");
        assert_eq!(SmimeCertSource::InboundMessage.as_str(), "inbound-message");

        assert_eq!(
            SmimeCertSource::from_db_str("vcard"),
            SmimeCertSource::Vcard
        );
        assert_eq!(
            SmimeCertSource::from_db_str("manual"),
            SmimeCertSource::Manual
        );
        assert_eq!(
            SmimeCertSource::from_db_str("inbound-message"),
            SmimeCertSource::InboundMessage
        );
        // Unknown values fall back to Manual rather than erroring.
        assert_eq!(
            SmimeCertSource::from_db_str("garbage"),
            SmimeCertSource::Manual
        );
    }
}

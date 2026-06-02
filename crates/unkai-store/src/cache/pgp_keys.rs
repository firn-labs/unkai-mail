//! OpenPGP public-key cache (#57).
//!
//! Stores the public keys of *recipients* (and signers of inbound mail)
//! so the Compose layer can answer "do we have a key for bob@example.com?"
//! and the receive layer can answer "is this signature from a key we
//! know?" without re-parsing vCards or asking the user.
//!
//! Each row carries the armored key blob plus a `source` string telling
//! us where it came from (`'vcard'` for keys auto-imported from a
//! Nextcloud contact's `KEY:` property, `'manual'` for paste-in-the-key
//! flows from the Compose key picker, `'inbound-message'` for keys
//! learned from a signed inbound mail in autocrypt-style discovery).
//! Provenance isn't security-load-bearing — the trust model is "key
//! fingerprint, period" — but the AccountSettings panel surfaces it so
//! the user can audit how each key got into their cache.
//!
//! The user's *own* private key lives in the OS keychain, not here —
//! see `credentials::store_pgp_private_key`.

use rusqlite::{OptionalExtension, params};
use tracing::info;

use crate::cache::{Cache, CacheError};

/// Tag describing where a cached public key came from.  Stored in the
/// `source` column as the lowercase kebab-case `as_str()` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgpKeySource {
    /// Auto-imported from a Nextcloud contact's vCard `KEY:` property.
    Vcard,
    /// Pasted in by the user via the Compose / AccountSettings UI.
    Manual,
    /// Learned from an inbound signed message (autocrypt-style).
    InboundMessage,
}

impl PgpKeySource {
    /// Serialised form used in the `pgp_public_keys.source` column.
    /// Matches the kebab-case convention used elsewhere in this schema
    /// (`replied_kind`, the unkai_crypto status enums, etc.).
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

/// One row in the `pgp_public_keys` table.  Public DTO; the SQL helpers
/// take and return this shape so callers don't have to thread five
/// arguments through every call.
#[derive(Debug, Clone)]
pub struct PgpPublicKeyRow {
    /// Uppercase hex fingerprint of the primary key — the canonical
    /// OpenPGP identifier.  Primary key in the table; uniqueness is
    /// the only invariant we enforce at the SQL layer.
    pub fingerprint: String,
    /// Primary user-id email address, when we could extract one from
    /// the key (or from the contact the key arrived with).  `None`
    /// when the key carries no user id with a parseable email — rare
    /// but legal.  Indexed for recipient lookups.
    pub email: Option<String>,
    /// Full `-----BEGIN PGP PUBLIC KEY BLOCK-----` ASCII as imported.
    pub armored_key: String,
    /// Where this key came from — see [`PgpKeySource`].
    pub source: PgpKeySource,
    /// Unix epoch seconds when the row landed.  Used by the
    /// AccountSettings panel to show "added 3 days ago" so the user
    /// can audit how fresh each cached key is.
    pub added_at: i64,
}

impl Cache {
    /// Insert (or replace by fingerprint) one public key.  Replace
    /// semantics let the auto-import path safely re-run on every
    /// contact sync without duplicate-key errors — the fingerprint
    /// is the canonical identity, so a re-arriving identical key is
    /// just a no-op in user-facing terms.
    pub fn upsert_pgp_public_key(&self, row: &PgpPublicKeyRow) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO pgp_public_keys
                (fingerprint, email, armored_key, source, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (fingerprint) DO UPDATE SET
                email        = excluded.email,
                armored_key  = excluded.armored_key,
                source       = excluded.source,
                added_at     = excluded.added_at",
            params![
                row.fingerprint,
                row.email,
                row.armored_key,
                row.source.as_str(),
                row.added_at,
            ],
        )?;
        info!(
            "Cached PGP public key fp={} email={} source={}",
            row.fingerprint,
            row.email.as_deref().unwrap_or("<none>"),
            row.source.as_str(),
        );
        Ok(())
    }

    /// Look up a cached public key by its fingerprint.  Returns `None`
    /// when we don't have a key with that fingerprint — the caller
    /// then decides whether to prompt the user to paste one in.
    pub fn get_pgp_public_key_by_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<Option<PgpPublicKeyRow>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT fingerprint, email, armored_key, source, added_at
                 FROM pgp_public_keys
                 WHERE fingerprint = ?1",
                params![fingerprint],
                row_from_columns,
            )
            .optional()?;
        Ok(row)
    }

    /// Look up every cached public key claiming a given email address.
    /// Returns a vec because real-world PGP users can rotate keys —
    /// the Compose layer should typically pick the most recently
    /// added (which is the order this query returns).
    pub fn get_pgp_public_keys_for_email(
        &self,
        email: &str,
    ) -> Result<Vec<PgpPublicKeyRow>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT fingerprint, email, armored_key, source, added_at
             FROM pgp_public_keys
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

    /// Enumerate every cached public key, newest first.  Used by the
    /// AccountSettings panel to render the "Known recipient keys" list.
    pub fn list_pgp_public_keys(&self) -> Result<Vec<PgpPublicKeyRow>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT fingerprint, email, armored_key, source, added_at
             FROM pgp_public_keys
             ORDER BY added_at DESC",
        )?;
        let rows = stmt.query_map([], row_from_columns)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Remove one cached public key by fingerprint.  Silently succeeds
    /// when the fingerprint isn't in the table — matches the keychain
    /// helpers' "no-op on missing" contract so the UI can fire-and-forget.
    pub fn delete_pgp_public_key(&self, fingerprint: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let removed = conn.execute(
            "DELETE FROM pgp_public_keys WHERE fingerprint = ?1",
            params![fingerprint],
        )?;
        if removed > 0 {
            info!("Deleted PGP public key fp={fingerprint}");
        }
        Ok(())
    }
}

fn row_from_columns(r: &rusqlite::Row<'_>) -> rusqlite::Result<PgpPublicKeyRow> {
    let source: String = r.get(3)?;
    Ok(PgpPublicKeyRow {
        fingerprint: r.get(0)?,
        email: r.get(1)?,
        armored_key: r.get(2)?,
        source: PgpKeySource::from_db_str(&source),
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

    fn sample_row(fp: &str, email: Option<&str>, source: PgpKeySource) -> PgpPublicKeyRow {
        PgpPublicKeyRow {
            fingerprint: fp.to_string(),
            email: email.map(|s| s.to_string()),
            armored_key: format!(
                "-----BEGIN PGP PUBLIC KEY BLOCK-----\n{fp}\n-----END PGP PUBLIC KEY BLOCK-----\n"
            ),
            source,
            added_at: 1_700_000_000,
        }
    }

    #[test]
    fn upsert_and_lookup_by_fingerprint() {
        let cache = open();
        let row = sample_row("AAAA1111", Some("alice@example.com"), PgpKeySource::Vcard);
        cache.upsert_pgp_public_key(&row).unwrap();

        let got = cache
            .get_pgp_public_key_by_fingerprint("AAAA1111")
            .unwrap()
            .expect("row should exist");
        assert_eq!(got.fingerprint, "AAAA1111");
        assert_eq!(got.email.as_deref(), Some("alice@example.com"));
        assert_eq!(got.source, PgpKeySource::Vcard);

        assert!(
            cache
                .get_pgp_public_key_by_fingerprint("missing")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn lookup_by_email_returns_newest_first() {
        let cache = open();
        cache
            .upsert_pgp_public_key(&PgpPublicKeyRow {
                added_at: 1_700_000_000,
                ..sample_row("AAA1", Some("alice@example.com"), PgpKeySource::Vcard)
            })
            .unwrap();
        cache
            .upsert_pgp_public_key(&PgpPublicKeyRow {
                added_at: 1_800_000_000, // newer
                ..sample_row("BBB2", Some("alice@example.com"), PgpKeySource::Manual)
            })
            .unwrap();

        let got = cache
            .get_pgp_public_keys_for_email("alice@example.com")
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].fingerprint, "BBB2", "newest first");
        assert_eq!(got[1].fingerprint, "AAA1");
    }

    #[test]
    fn upsert_replaces_existing_row_for_same_fingerprint() {
        let cache = open();
        cache
            .upsert_pgp_public_key(&sample_row(
                "CCC3",
                Some("old@example.com"),
                PgpKeySource::Vcard,
            ))
            .unwrap();
        cache
            .upsert_pgp_public_key(&sample_row(
                "CCC3",
                Some("new@example.com"),
                PgpKeySource::Manual,
            ))
            .unwrap();

        let got = cache
            .get_pgp_public_key_by_fingerprint("CCC3")
            .unwrap()
            .unwrap();
        assert_eq!(got.email.as_deref(), Some("new@example.com"));
        assert_eq!(got.source, PgpKeySource::Manual);

        // And we end up with one row, not two.
        let all = cache.list_pgp_public_keys().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn delete_is_noop_on_missing_fingerprint() {
        let cache = open();
        cache.delete_pgp_public_key("does-not-exist").unwrap();
    }

    #[test]
    fn source_round_trip_via_string() {
        assert_eq!(PgpKeySource::Vcard.as_str(), "vcard");
        assert_eq!(PgpKeySource::Manual.as_str(), "manual");
        assert_eq!(PgpKeySource::InboundMessage.as_str(), "inbound-message");

        assert_eq!(PgpKeySource::from_db_str("vcard"), PgpKeySource::Vcard);
        assert_eq!(PgpKeySource::from_db_str("manual"), PgpKeySource::Manual);
        assert_eq!(
            PgpKeySource::from_db_str("inbound-message"),
            PgpKeySource::InboundMessage
        );
        // Unknown values fall back to Manual rather than erroring.
        assert_eq!(PgpKeySource::from_db_str("garbage"), PgpKeySource::Manual);
    }
}

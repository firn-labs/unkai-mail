//! Contacts cache and sync-state persistence.
//!
//! The shapes here mirror the CardDAV layer's outputs, but kept in
//! their own struct (`ContactRow`) so the store crate doesn't have to
//! depend on `unkai-carddav`. The Tauri layer converts between the
//! two — a tiny field-for-field map.
//!
//! # Why store the raw vCard
//!
//! Two reasons:
//!
//! 1. **Forward-compat**: when we later want to expose more vCard
//!    fields (birthday, addresses, categories), we can re-extract
//!    them from the cached row without re-syncing every contact from
//!    the server. Important on big address books.
//! 2. **Round-trip safety**: if we ever add edit support, we need the
//!    exact vCard text we last sent so we can produce a sensible diff
//!    instead of regenerating from a lossy projection.

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{OptionalExtension, params};

use unkai_core::models::{Contact, ContactAddress, ContactEmail, ContactPhone};

use crate::cache::{Cache, CacheError};

/// One contact ready for upsert. Mirrors `unkai_carddav::sync::RawContact`
/// without the dependency.
#[derive(Debug, Clone)]
pub struct ContactRow {
    pub href: String,
    pub etag: String,
    pub vcard_uid: String,
    pub display_name: String,
    /// Email addresses paired with the vCard `EMAIL;TYPE=…` kind
    /// hint. Same backward-compat story as `phones` — JSON column
    /// reads tolerate the legacy `Vec<String>` shape.
    pub emails: Vec<ContactEmail>,
    /// Phone numbers paired with the vCard `TEL;TYPE=…` kind hint.
    /// Stored as JSON in `phones_json`; reads tolerate the legacy
    /// `Vec<String>` shape so existing rows keep working until the
    /// next sync rewrites them in the new shape.
    pub phones: Vec<ContactPhone>,
    pub organization: Option<String>,
    pub photo_mime: Option<String>,
    pub photo_data: Option<Vec<u8>>,
    /// Job title (vCard `TITLE`).
    pub title: Option<String>,
    /// Birthday (vCard `BDAY`) as the literal vCard string —
    /// formats vary, the UI renders verbatim.
    pub birthday: Option<String>,
    /// Free-form note (vCard `NOTE`).
    pub note: Option<String>,
    pub addresses: Vec<ContactAddress>,
    pub urls: Vec<String>,
    pub vcard_raw: String,
    /// vCard `KIND` (RFC 6350 §6.1.4) — `"group"` flags this row
    /// as a contact group / mailing list.  Empty for individuals.
    pub kind: String,
    /// `MEMBER` URI list pulled from a `KIND:group` vCard.  We
    /// preserve the raw URI shape (`urn:uuid:<uid>`) so the
    /// resolver can match members against other vCards.  Empty
    /// for non-group rows.
    pub member_uids: Vec<String>,
    /// `CATEGORIES` tag list — drives the contacts sidebar's
    /// Kontaktgruppen rows + the virtual mailing-list view
    /// (#133 redesign).
    pub categories: Vec<String>,
}

/// Sync bookmark for one addressbook.
#[derive(Debug, Clone)]
pub struct AddressbookSyncState {
    pub display_name: Option<String>,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// Server-side bookkeeping for one cached contact, returned from
/// `get_contact_server_handle`. The Tauri layer needs these fields
/// to do a PUT or DELETE — the user-facing `Contact` struct hides
/// them deliberately since the UI shouldn't touch hrefs and etags.
#[derive(Debug, Clone)]
pub struct ContactServerHandle {
    pub nextcloud_account_id: String,
    pub addressbook: String,
    pub vcard_uid: String,
    pub href: String,
    pub etag: String,
    pub vcard_raw: String,
}

impl Cache {
    // ── Contacts ────────────────────────────────────────────────

    /// Apply one CardDAV sync delta in a single transaction.
    ///
    /// `upserts` are added or changed contacts; `deleted_hrefs` are
    /// resources the server reported as gone (404 in the sync-collection
    /// response). The new sync token, if any, is persisted in the
    /// `addressbook_sync_state` row alongside.
    ///
    /// All-or-nothing: a failure inside the transaction leaves the
    /// previous cache state intact, so we never half-apply a delta.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_contact_delta(
        &self,
        nc_account_id: &str,
        addressbook: &str,
        addressbook_display_name: Option<&str>,
        upserts: &[ContactRow],
        deleted_hrefs: &[String],
        new_sync_token: Option<&str>,
        new_ctag: Option<&str>,
    ) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let now = Utc::now().timestamp();

        // Deletes by href — match within the addressbook to avoid
        // ever accidentally clobbering a contact in another book that
        // shares the same vcard UID (rare but theoretically possible).
        if !deleted_hrefs.is_empty() {
            let mut stmt = tx.prepare(
                "DELETE FROM contacts
                 WHERE nextcloud_account_id = ?1
                   AND addressbook = ?2
                   AND href = ?3",
            )?;
            for href in deleted_hrefs {
                stmt.execute(params![nc_account_id, addressbook, href])?;
            }
        }

        if !upserts.is_empty() {
            let mut stmt = tx.prepare(
                "INSERT INTO contacts
                    (id, nextcloud_account_id, addressbook, vcard_uid, href, etag,
                     display_name, emails_json, phones_json, organization,
                     photo_mime, photo_data, vcard_raw, cached_at,
                     title, birthday, note, addresses_json, urls_json,
                     kind, member_uids_json, categories_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
                 ON CONFLICT (nextcloud_account_id, addressbook, vcard_uid) DO UPDATE SET
                    href             = excluded.href,
                    etag             = excluded.etag,
                    display_name     = excluded.display_name,
                    emails_json      = excluded.emails_json,
                    phones_json      = excluded.phones_json,
                    organization     = excluded.organization,
                    photo_mime       = excluded.photo_mime,
                    photo_data       = excluded.photo_data,
                    vcard_raw        = excluded.vcard_raw,
                    cached_at        = excluded.cached_at,
                    title            = excluded.title,
                    birthday         = excluded.birthday,
                    note             = excluded.note,
                    addresses_json   = excluded.addresses_json,
                    urls_json        = excluded.urls_json,
                    kind             = excluded.kind,
                    member_uids_json = excluded.member_uids_json,
                    categories_json  = excluded.categories_json",
            )?;
            for c in upserts {
                let id = format!("{nc_account_id}::{}", c.vcard_uid);
                let emails = serde_json::to_string(&c.emails).unwrap_or_else(|_| "[]".into());
                let phones = serde_json::to_string(&c.phones).unwrap_or_else(|_| "[]".into());
                let addresses = serde_json::to_string(&c.addresses).unwrap_or_else(|_| "[]".into());
                let urls = serde_json::to_string(&c.urls).unwrap_or_else(|_| "[]".into());
                let members = serde_json::to_string(&c.member_uids).unwrap_or_else(|_| "[]".into());
                let categories =
                    serde_json::to_string(&c.categories).unwrap_or_else(|_| "[]".into());
                stmt.execute(params![
                    id,
                    nc_account_id,
                    addressbook,
                    c.vcard_uid,
                    c.href,
                    c.etag,
                    c.display_name,
                    emails,
                    phones,
                    c.organization,
                    c.photo_mime,
                    c.photo_data,
                    c.vcard_raw,
                    now,
                    c.title,
                    c.birthday,
                    c.note,
                    addresses,
                    urls,
                    c.kind,
                    members,
                    categories,
                ])?;
            }
        }

        // Sync state — upsert the bookmark even when the delta itself
        // was empty, so an empty incremental run still bumps
        // last_synced_at.
        tx.execute(
            "INSERT INTO addressbook_sync_state
                (nextcloud_account_id, addressbook, display_name, sync_token, ctag, last_synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (nextcloud_account_id, addressbook) DO UPDATE SET
                display_name   = COALESCE(excluded.display_name, addressbook_sync_state.display_name),
                sync_token     = COALESCE(excluded.sync_token, addressbook_sync_state.sync_token),
                ctag           = COALESCE(excluded.ctag, addressbook_sync_state.ctag),
                last_synced_at = excluded.last_synced_at",
            params![
                nc_account_id,
                addressbook,
                addressbook_display_name,
                new_sync_token,
                new_ctag,
                now,
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Most-recent `last_synced_at` across every addressbook for the
    /// given Nextcloud account, in UTC. `Ok(None)` means we've never
    /// completed a sync for this account — the settings UI uses that
    /// to show "Never synced" rather than a misleading "0s ago".
    pub fn latest_addressbook_sync_at(
        &self,
        nc_account_id: &str,
    ) -> Result<Option<DateTime<Utc>>, CacheError> {
        let conn = self.conn()?;
        let ts: Option<i64> = conn
            .query_row(
                "SELECT MAX(last_synced_at)
                 FROM addressbook_sync_state
                 WHERE nextcloud_account_id = ?1",
                params![nc_account_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(ts.and_then(|t| Utc.timestamp_opt(t, 0).single()))
    }

    /// Read the addressbook sync bookmark, if any.
    pub fn get_addressbook_sync_state(
        &self,
        nc_account_id: &str,
        addressbook: &str,
    ) -> Result<Option<AddressbookSyncState>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT display_name, sync_token, ctag, last_synced_at
                 FROM addressbook_sync_state
                 WHERE nextcloud_account_id = ?1 AND addressbook = ?2",
                params![nc_account_id, addressbook],
                |r| {
                    let ts: Option<i64> = r.get(3)?;
                    Ok(AddressbookSyncState {
                        display_name: r.get(0)?,
                        sync_token: r.get(1)?,
                        ctag: r.get(2)?,
                        last_synced_at: ts.and_then(|t| Utc.timestamp_opt(t, 0).single()),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// All contacts, alphabetised, optionally scoped to a single
    /// Nextcloud account. Powers the contacts list view.
    ///
    /// **Deliberately omits photo bytes** — `photo_data` is always
    /// returned as `None`. Photos can be 50–500 KB each and Tauri
    /// serialises them as JSON number arrays (3–4× bloat), so
    /// shipping them in the list payload turns a 200-contact
    /// addressbook into 30+ MB of IPC traffic. `photo_mime` is kept
    /// as a presence flag; the UI uses `get_contact_photo` to fetch
    /// the bytes on demand for whichever rows it actually paints.
    pub fn list_contacts(&self, nc_account_id: Option<&str>) -> Result<Vec<Contact>, CacheError> {
        let conn = self.conn()?;
        let mut stmt;
        let rows = match nc_account_id {
            Some(nc) => {
                stmt = conn.prepare(
                    "SELECT id, nextcloud_account_id, display_name, emails_json,
                            phones_json, organization, photo_mime,
                            title, birthday, note, addresses_json, urls_json,
                            categories_json, addressbook
                     FROM contacts
                     WHERE nextcloud_account_id = ?1
                       AND COALESCE(kind, '') != 'group'
                     ORDER BY display_name COLLATE NOCASE",
                )?;
                stmt.query_map(params![nc], row_to_contact_no_photo)?
            }
            None => {
                stmt = conn.prepare(
                    "SELECT id, nextcloud_account_id, display_name, emails_json,
                            phones_json, organization, photo_mime,
                            title, birthday, note, addresses_json, urls_json,
                            categories_json, addressbook
                     FROM contacts
                     WHERE COALESCE(kind, '') != 'group'
                     ORDER BY display_name COLLATE NOCASE",
                )?;
                stmt.query_map([], row_to_contact_no_photo)?
            }
        };
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Fetch one contact's photo bytes by app-side id. Returns
    /// `Ok(None)` when the contact has no photo (or doesn't exist),
    /// so the UI can render its initial-letter placeholder without a
    /// distinct error path.
    pub fn get_contact_photo(
        &self,
        contact_id: &str,
    ) -> Result<Option<(String, Vec<u8>)>, CacheError> {
        let conn = self.conn()?;
        let row: Option<(Option<String>, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT photo_mime, photo_data FROM contacts WHERE id = ?1",
                params![contact_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(mime, bytes)| match (mime, bytes) {
            (Some(m), Some(b)) if !b.is_empty() => Some((m, b)),
            _ => None,
        }))
    }

    /// Re-parse `vcard_raw` for every contact and rewrite
    /// `addresses_json` when the freshly-parsed result differs
    /// from what's stored.
    ///
    /// Why so aggressive: two stale-data sources accumulate over
    /// time —
    ///   1. The `ALTER TABLE` migration that added `addresses_json`
    ///      defaulted it to `'[]'` for every existing row.  CardDAV
    ///      delta-sync only re-pulls contacts that have changed in
    ///      NC since the last token, so unchanged rows kept the
    ///      empty default forever even though their cached body
    ///      held the original `ADR`.
    ///   2. Bugs in the vCard parser (e.g. a missed group prefix
    ///      like `item1.ADR`) caused the parser to silently drop
    ///      the address at sync time, again writing `'[]'` (or a
    ///      stub) to the column.  The contact's `vcard_raw` still
    ///      has the address, but every subsequent list-render
    ///      reads the empty cached value.
    ///
    /// Walking every row and re-parsing fixes both classes in one
    /// pass: when the parser is later corrected, the next boot
    /// rewrites the affected rows; once corrected, the comparison
    /// short-circuits and the loop is a no-op.
    ///
    /// `parse` is injected so this crate stays a dep leaf and
    /// doesn't grow a `unkai-carddav` dependency.  Returns the
    /// number of rows actually rewritten.
    pub fn backfill_addresses<F>(&self, parse: F) -> Result<usize, CacheError>
    where
        F: Fn(&str) -> Option<Vec<ContactAddress>>,
    {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, vcard_raw, addresses_json
             FROM contacts
             WHERE vcard_raw IS NOT NULL AND vcard_raw != ''",
        )?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        let mut updated = 0usize;
        for (id, raw, current_json) in rows {
            let Some(addresses) = parse(&raw) else {
                continue;
            };
            let addresses_json = serde_json::to_string(&addresses).unwrap_or_else(|_| "[]".into());
            // Skip when the parsed result matches what's already
            // stored.  Cheap string compare; serde's output is
            // deterministic for a given input shape so this
            // catches every steady-state row.
            if addresses_json == current_json {
                continue;
            }
            conn.execute(
                "UPDATE contacts SET addresses_json = ?2 WHERE id = ?1",
                params![id, addresses_json],
            )?;
            updated += 1;
        }
        Ok(updated)
    }

    /// Substring search over name + email for autocomplete.
    ///
    /// Matches `display_name` OR any email containing `query` (case
    /// insensitive). Excludes rows with no email addresses — the
    /// compose autocomplete needs *something* to fill into the field,
    /// so a phone-only contact is just noise here. Caps results at
    /// `limit` so a typo that matches half the address book doesn't
    /// tank the UI.
    pub fn search_contacts(&self, query: &str, limit: u32) -> Result<Vec<Contact>, CacheError> {
        let conn = self.conn()?;
        let needle = format!("%{}%", query.replace('%', r"\%").replace('_', r"\_"));
        // emails_json is the stringified JSON array. "[]" is the
        // canonical empty form (see apply_contact_delta), so excluding
        // it filters phone/photo-only rows reliably.
        //
        // Same photo-omission story as `list_contacts` — autocomplete
        // doesn't render avatars, so shipping bytes is pure waste.
        let mut stmt = conn.prepare(
            "SELECT id, nextcloud_account_id, display_name, emails_json,
                    phones_json, organization, photo_mime,
                    title, birthday, note, addresses_json, urls_json,
                    categories_json, addressbook
             FROM contacts
             WHERE emails_json != '[]'
               AND COALESCE(kind, '') != 'group'
               AND (display_name LIKE ?1 ESCAPE '\\' COLLATE NOCASE
                    OR emails_json  LIKE ?1 ESCAPE '\\' COLLATE NOCASE)
             ORDER BY display_name COLLATE NOCASE
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![needle, limit as i64], row_to_contact_no_photo)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Number of contacts cached for a Nextcloud account — cheap to
    /// surface in the Settings UI alongside "Sync now".
    pub fn count_contacts(&self, nc_account_id: &str) -> Result<u32, CacheError> {
        let conn = self.conn()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM contacts WHERE nextcloud_account_id = ?1",
            params![nc_account_id],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// Look up the server-side handle (href + etag + addressbook + raw
    /// vCard) for a single cached contact by its app-side id.
    ///
    /// Returns `Ok(None)` if the row isn't cached — the caller treats
    /// that as "stale UI; trigger a refresh and try again".
    pub fn get_contact_server_handle(
        &self,
        contact_id: &str,
    ) -> Result<Option<ContactServerHandle>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT nextcloud_account_id, addressbook, vcard_uid, href, etag, vcard_raw
                 FROM contacts
                 WHERE id = ?1",
                params![contact_id],
                |r| {
                    Ok(ContactServerHandle {
                        nextcloud_account_id: r.get(0)?,
                        addressbook: r.get(1)?,
                        vcard_uid: r.get(2)?,
                        href: r.get(3)?,
                        etag: r.get(4)?,
                        vcard_raw: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Insert (or replace) a single contact row outside the
    /// sync-collection delta path. Used by the create/update Tauri
    /// commands after a successful PUT to Nextcloud — we already
    /// have the new etag and don't want to wait for the next sync
    /// to see our own write.
    ///
    /// Does not touch `addressbook_sync_state`; the next regular
    /// sync will move the token forward and will simply find no
    /// changes for the row we just wrote (or report it as our own
    /// edit, also fine).
    pub fn upsert_single_contact(
        &self,
        nc_account_id: &str,
        addressbook: &str,
        row: &ContactRow,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let id = format!("{nc_account_id}::{}", row.vcard_uid);
        let emails = serde_json::to_string(&row.emails).unwrap_or_else(|_| "[]".into());
        let phones = serde_json::to_string(&row.phones).unwrap_or_else(|_| "[]".into());
        let addresses = serde_json::to_string(&row.addresses).unwrap_or_else(|_| "[]".into());
        let urls = serde_json::to_string(&row.urls).unwrap_or_else(|_| "[]".into());
        let now = Utc::now().timestamp();
        let members = serde_json::to_string(&row.member_uids).unwrap_or_else(|_| "[]".into());
        let categories = serde_json::to_string(&row.categories).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT INTO contacts
                (id, nextcloud_account_id, addressbook, vcard_uid, href, etag,
                 display_name, emails_json, phones_json, organization,
                 photo_mime, photo_data, vcard_raw, cached_at,
                 title, birthday, note, addresses_json, urls_json,
                 kind, member_uids_json, categories_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
             ON CONFLICT (nextcloud_account_id, addressbook, vcard_uid) DO UPDATE SET
                href             = excluded.href,
                etag             = excluded.etag,
                display_name     = excluded.display_name,
                emails_json      = excluded.emails_json,
                phones_json      = excluded.phones_json,
                organization     = excluded.organization,
                photo_mime       = excluded.photo_mime,
                photo_data       = excluded.photo_data,
                vcard_raw        = excluded.vcard_raw,
                cached_at        = excluded.cached_at,
                title            = excluded.title,
                birthday         = excluded.birthday,
                note             = excluded.note,
                addresses_json   = excluded.addresses_json,
                urls_json        = excluded.urls_json,
                kind             = excluded.kind,
                member_uids_json = excluded.member_uids_json,
                categories_json  = excluded.categories_json",
            params![
                id,
                nc_account_id,
                addressbook,
                row.vcard_uid,
                row.href,
                row.etag,
                row.display_name,
                emails,
                phones,
                row.organization,
                row.photo_mime,
                row.photo_data,
                row.vcard_raw,
                now,
                row.title,
                row.birthday,
                row.note,
                addresses,
                urls,
                row.kind,
                members,
                categories,
            ],
        )?;
        Ok(())
    }

    /// One-shot backfill for the `categories_json` column —
    /// callers pass a closure that can extract CATEGORIES out
    /// of a cached `vcard_raw`.  Rows that already have a
    /// non-`'[]'` categories list are skipped, so subsequent
    /// calls are O(matched) and the IPC paths can call this
    /// idempotently every render without paying for already-
    /// hydrated rows.
    pub fn backfill_categories<F>(&self, parse: F) -> Result<u32, CacheError>
    where
        F: Fn(&str) -> Vec<String>,
    {
        let conn = self.conn()?;
        let pairs: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, vcard_raw FROM contacts
                 WHERE COALESCE(categories_json, '[]') = '[]'
                   AND vcard_raw LIKE '%CATEGORIES%'",
            )?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut updated = 0u32;
        for (id, raw) in pairs {
            let cats = parse(&raw);
            if cats.is_empty() {
                continue;
            }
            let json = serde_json::to_string(&cats).unwrap_or_else(|_| "[]".into());
            conn.execute(
                "UPDATE contacts SET categories_json = ?1 WHERE id = ?2",
                params![json, id],
            )?;
            updated += 1;
        }
        Ok(updated)
    }

    // ── Categories / Kontaktgruppen (#133 redesign) ─────────────

    /// Distinct CATEGORIES across every cached contact.  Each
    /// row carries the count of contacts tagged with the
    /// category; the unified mailing-list view derives a
    /// virtual mailing list per row.  Empty when no card has a
    /// CATEGORIES tag.
    pub fn list_contact_categories(&self) -> Result<Vec<(String, u32)>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT categories_json FROM contacts
             WHERE COALESCE(kind, '') != 'group'
               AND categories_json != '[]'",
        )?;
        let rows = stmt.query_map([], |r| {
            let v: String = r.get(0)?;
            Ok(v)
        })?;
        let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for r in rows {
            let json = r?;
            let cats: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
            for c in cats {
                let trimmed = c.trim();
                if trimmed.is_empty() {
                    continue;
                }
                *counts.entry(trimmed.to_string()).or_insert(0) += 1;
            }
        }
        let mut out: Vec<(String, u32)> = counts.into_iter().collect();
        out.sort_by(|a, b| {
            a.0.to_lowercase()
                .cmp(&b.0.to_lowercase())
                .then_with(|| a.0.cmp(&b.0))
        });
        Ok(out)
    }

    /// Return contacts that carry the given CATEGORY (case
    /// sensitive — that's what NC's UI does).  Walks every
    /// non-group contact row and matches against the parsed
    /// `c.categories` list — no LIKE pre-filter, since the
    /// JSON-substring approach was missing rows whose
    /// `categories_json` column hadn't been backfilled yet
    /// (the column LIKE-matched `'[]'` for those, even when
    /// the underlying vCard's CATEGORIES line was non-empty).
    /// Iterating every row is fine at typical addressbook
    /// sizes — one cheap SELECT per Lists-tab open, not per
    /// keystroke.
    pub fn list_contacts_with_category(&self, category: &str) -> Result<Vec<Contact>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, nextcloud_account_id, display_name, emails_json,
                    phones_json, organization, photo_mime,
                    title, birthday, note, addresses_json, urls_json,
                    categories_json, addressbook
             FROM contacts
             WHERE COALESCE(kind, '') != 'group'
             ORDER BY display_name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], row_to_contact_no_photo)?;
        let mut out = Vec::new();
        for r in rows {
            let c = r?;
            if c.categories.iter().any(|cc| cc == category) {
                out.push(c);
            }
        }
        Ok(out)
    }

    /// Look up one contact's server handle by bare vcard UID +
    /// NC account.  Used by the category CRUD path so we can
    /// PUT a rewritten vCard without round-tripping through
    /// the composite app-side id.
    pub fn get_contact_handle_by_uid(
        &self,
        nc_account_id: &str,
        vcard_uid: &str,
    ) -> Result<Option<ContactServerHandle>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT nextcloud_account_id, addressbook, vcard_uid, href, etag, vcard_raw
                 FROM contacts
                 WHERE nextcloud_account_id = ?1 AND vcard_uid = ?2",
                params![nc_account_id, vcard_uid],
                |r| {
                    Ok(ContactServerHandle {
                        nextcloud_account_id: r.get(0)?,
                        addressbook: r.get(1)?,
                        vcard_uid: r.get(2)?,
                        href: r.get(3)?,
                        etag: r.get(4)?,
                        vcard_raw: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Per-row local overlay for the mailing-list view: when
    /// `suppressed = 1` the row is dropped from the autocomplete
    /// AND from the Mailing Lists tab.  Keyed by the unified id
    /// (`cat:<name>` / `group:<id>` / `team:<id>` / `list:<uid>`).
    pub fn get_mailing_list_suppressed(
        &self,
    ) -> Result<std::collections::HashSet<String>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM mailing_list_settings
             WHERE hidden_from_autocomplete = 1",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for r in rows {
            out.insert(r?);
        }
        Ok(out)
    }

    pub fn set_mailing_list_suppressed(
        &self,
        id: &str,
        suppressed: bool,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO mailing_list_settings (id, hidden_from_autocomplete)
             VALUES (?1, ?2)
             ON CONFLICT (id) DO UPDATE SET hidden_from_autocomplete = excluded.hidden_from_autocomplete",
            params![id, suppressed as i64],
        )?;
        Ok(())
    }

    /// Local-only emoji avatar overlay for the unified mailing-list
    /// view, keyed by `cat:<name>` / `list:<uid>` / `team:<id>` /
    /// `group:<id>`.  Empty map when no overrides exist.
    pub fn get_mailing_list_emojis(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, emoji FROM mailing_list_settings
             WHERE emoji IS NOT NULL AND emoji != ''",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (k, v) = r?;
            out.insert(k, v);
        }
        Ok(out)
    }

    pub fn set_mailing_list_emoji(&self, id: &str, emoji: Option<&str>) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO mailing_list_settings (id, hidden_from_autocomplete, emoji)
             VALUES (?1, 0, ?2)
             ON CONFLICT (id) DO UPDATE SET emoji = excluded.emoji",
            params![id, emoji.filter(|s| !s.is_empty())],
        )?;
        Ok(())
    }

    /// Rename one entry in `mailing_list_settings` so the local
    /// hide / emoji overlay survives a category rename (the id
    /// changes from `cat:<old>` to `cat:<new>`).
    pub fn rename_mailing_list_setting(
        &self,
        old_id: &str,
        new_id: &str,
    ) -> Result<(), CacheError> {
        if old_id == new_id {
            return Ok(());
        }
        let conn = self.conn()?;
        conn.execute(
            "UPDATE mailing_list_settings SET id = ?2 WHERE id = ?1",
            params![old_id, new_id],
        )?;
        Ok(())
    }

    // ── Contact groups (#133, #113) ────────────────────────────

    /// List every cached contact group across the user's address
    /// books.  Returns one row per `KIND:group` vCard, with the
    /// local-only `group_emoji` and `group_hidden` overlay applied
    /// so the UI can render hidden state without a second query.
    pub fn list_contact_groups(&self) -> Result<Vec<unkai_core::models::ContactGroup>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, nextcloud_account_id, display_name,
                    member_uids_json, group_emoji, group_hidden
             FROM contacts
             WHERE COALESCE(kind, '') = 'group'
             ORDER BY display_name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let nc: String = r.get(1)?;
            let name: String = r.get(2)?;
            let members_json: String = r.get(3)?;
            let emoji: Option<String> = r.get(4)?;
            let hidden: i64 = r.get(5)?;
            let raw_uris: Vec<String> = serde_json::from_str(&members_json).unwrap_or_default();
            // Strip the `urn:uuid:` prefix that vCard 4 prescribes
            // so the frontend gets bare UIDs ready to match against
            // contact rows.  Anything that isn't a urn:uuid form
            // (e.g. `mailto:` for guest members) is preserved
            // verbatim — the caller can pattern-match.
            let member_uids: Vec<String> = raw_uris
                .into_iter()
                .map(|u| {
                    u.strip_prefix("urn:uuid:")
                        .map(|s| s.to_string())
                        .unwrap_or(u)
                })
                .collect();
            Ok(unkai_core::models::ContactGroup {
                id,
                nextcloud_account_id: nc,
                display_name: name,
                member_uids,
                emoji: emoji.filter(|s| !s.is_empty()),
                hidden: hidden != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Toggle the local `group_hidden` flag for one group.
    pub fn set_contact_group_hidden(&self, group_id: &str, hidden: bool) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE contacts SET group_hidden = ?2 WHERE id = ?1",
            params![group_id, hidden as i64],
        )?;
        Ok(())
    }

    /// Set (or clear) the local `group_emoji` overlay for one group.
    pub fn set_contact_group_emoji(
        &self,
        group_id: &str,
        emoji: Option<&str>,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE contacts SET group_emoji = ?2 WHERE id = ?1",
            params![group_id, emoji.filter(|s| !s.is_empty())],
        )?;
        Ok(())
    }

    /// Resolve a list of bare vCard UIDs to lightweight contact
    /// rows (id + display name + first email) — used by the
    /// AddressAutocomplete to expand a group selection into its
    /// individual recipients without round-tripping through the
    /// full `Contact` shape (the autocomplete only needs an
    /// addressable email per member).  `nc_account_id` scopes
    /// the lookup so a group on server A can't accidentally pull
    /// in a same-UID member from server B.
    pub fn resolve_group_members(
        &self,
        nc_account_id: &str,
        member_uids: &[String],
    ) -> Result<Vec<(String, String, String)>, CacheError> {
        if member_uids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn()?;
        let placeholders = member_uids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, display_name, emails_json
             FROM contacts
             WHERE nextcloud_account_id = ?1
               AND COALESCE(kind, '') != 'group'
               AND vcard_uid IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(1 + member_uids.len());
        params_vec.push(&nc_account_id);
        for u in member_uids {
            params_vec.push(u);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec), |r| {
            let id: String = r.get(0)?;
            let name: String = r.get(1)?;
            let emails_json: String = r.get(2)?;
            let emails: Vec<ContactEmail> = serde_json::from_str(&emails_json).unwrap_or_default();
            let first_email = emails
                .into_iter()
                .next()
                .map(|e| e.value)
                .unwrap_or_default();
            Ok((id, name, first_email))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Remove one contact by its app-side id.
    pub fn delete_contact_by_id(&self, contact_id: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM contacts WHERE id = ?1", params![contact_id])?;
        Ok(())
    }

    /// Drop all contacts and sync state for a Nextcloud account —
    /// called when the user disconnects that account.
    pub fn wipe_nextcloud_contacts(&self, nc_account_id: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM contacts WHERE nextcloud_account_id = ?1",
            params![nc_account_id],
        )?;
        conn.execute(
            "DELETE FROM addressbook_sync_state WHERE nextcloud_account_id = ?1",
            params![nc_account_id],
        )?;
        Ok(())
    }
}

/// Map a row that excludes the `photo_data` column. `photo_mime` is
/// kept (column index 6) so the UI knows whether a photo exists
/// without having to ship the bytes; `photo_data` is forced to
/// `None`. Pair with the SELECT lists in `list_contacts` and
/// `search_contacts`.
fn row_to_contact_no_photo(r: &rusqlite::Row<'_>) -> rusqlite::Result<Contact> {
    let emails_json: String = r.get(3)?;
    let phones_json: String = r.get(4)?;
    let addresses_json: String = r.get(10)?;
    let urls_json: String = r.get(11)?;
    let categories_json: String = r.get(12)?;
    let addressbook: String = r.get(13)?;
    Ok(Contact {
        id: r.get(0)?,
        nextcloud_account_id: r.get(1)?,
        addressbook,
        display_name: r.get(2)?,
        email: decode_emails(&emails_json),
        phone: decode_phones(&phones_json),
        organization: r.get(5)?,
        photo_mime: r.get(6)?,
        photo_data: None,
        title: r.get(7)?,
        birthday: r.get(8)?,
        note: r.get(9)?,
        addresses: serde_json::from_str(&addresses_json).unwrap_or_default(),
        urls: serde_json::from_str(&urls_json).unwrap_or_default(),
        kind: String::new(),
        categories: serde_json::from_str(&categories_json).unwrap_or_default(),
        // #143 — extended vCard 4 fields aren't materialised in
        // this cache row.  Callers that surface the contact form
        // (`row_to_contact` in src-tauri) re-parse `vcard_raw` to
        // recover them; this lighter list-rendering path leaves
        // them defaulted because the autocomplete / search /
        // list-view UIs don't show them.
        structured_name: unkai_core::models::StructuredName::default(),
        nickname: None,
        anniversary: None,
        gender: None,
        impp: Vec::new(),
        role: None,
        languages: Vec::new(),
        geo: None,
        timezone: None,
        keys: Vec::new(),
    })
}

/// Read `phones_json` tolerantly. The new shape is `[{kind, value}]`
/// (vCard `TEL;TYPE=…`); rows written before this column was typed
/// have the old `[String]` shape, which we lift to a typed array
/// with `kind = "other"` so no number ever vanishes from the UI.
/// On the next sync, the rewrite from CardDAV puts the proper kind
/// in place.
fn decode_phones(json: &str) -> Vec<ContactPhone> {
    if let Ok(typed) = serde_json::from_str::<Vec<ContactPhone>>(json) {
        return typed;
    }
    if let Ok(plain) = serde_json::from_str::<Vec<String>>(json) {
        return plain
            .into_iter()
            .map(|v| ContactPhone {
                kind: "other".to_string(),
                value: v,
            })
            .collect();
    }
    Vec::new()
}

/// Mirror of `decode_phones` for `emails_json`. Same shape evolution
/// (`[String]` → `[{kind, value}]`), same legacy-rows-stay-readable
/// guarantee.
fn decode_emails(json: &str) -> Vec<ContactEmail> {
    if let Ok(typed) = serde_json::from_str::<Vec<ContactEmail>>(json) {
        return typed;
    }
    if let Ok(plain) = serde_json::from_str::<Vec<String>>(json) {
        return plain
            .into_iter()
            .map(|v| ContactEmail {
                kind: "other".to_string(),
                value: v,
            })
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn open_test_cache() -> Cache {
        Cache::open_in_memory().expect("open in-memory cache")
    }

    fn row(uid: &str, name: &str, email: &str) -> ContactRow {
        ContactRow {
            href: format!("/dav/{uid}.vcf"),
            etag: format!("etag-{uid}"),
            vcard_uid: uid.into(),
            display_name: name.into(),
            emails: vec![ContactEmail {
                kind: "other".into(),
                value: email.into(),
            }],
            phones: vec![],
            organization: None,
            photo_mime: None,
            photo_data: None,
            title: None,
            birthday: None,
            note: None,
            addresses: Vec::new(),
            urls: Vec::new(),
            vcard_raw: format!("BEGIN:VCARD\r\nUID:{uid}\r\nEND:VCARD\r\n"),
            kind: String::new(),
            member_uids: Vec::new(),
            categories: Vec::new(),
        }
    }

    #[test]
    fn search_excludes_contacts_without_email() {
        let cache = open_test_cache();
        let phone_only = ContactRow {
            emails: vec![],
            phones: vec![ContactPhone {
                kind: "cell".into(),
                value: "+1 555 1234".into(),
            }],
            ..row("u9", "Phone Only", "")
        };
        cache
            .apply_contact_delta("nc1", "contacts", None, &[phone_only], &[], None, None)
            .unwrap();
        // Substring of the display name still finds nothing because the
        // row has no emails to autocomplete.
        assert!(cache.search_contacts("phone", 10).unwrap().is_empty());
    }

    #[test]
    fn upsert_then_search_finds_by_name_and_email() {
        let cache = open_test_cache();
        let upserts = vec![
            row("u1", "Alice Wonder", "alice@example.com"),
            row("u2", "Bob Marley", "bob@reggae.com"),
        ];
        cache
            .apply_contact_delta(
                "nc1",
                "contacts",
                Some("Contacts"),
                &upserts,
                &[],
                Some("tok-1"),
                Some("c1"),
            )
            .unwrap();

        // Hit by name
        let r = cache.search_contacts("alice", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].display_name, "Alice Wonder");

        // Hit by email
        let r = cache.search_contacts("reggae", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].email.len(), 1);
        assert_eq!(r[0].email[0].value, "bob@reggae.com");

        // count_contacts
        assert_eq!(cache.count_contacts("nc1").unwrap(), 2);

        // sync state stuck around
        let s = cache
            .get_addressbook_sync_state("nc1", "contacts")
            .unwrap()
            .unwrap();
        assert_eq!(s.sync_token.as_deref(), Some("tok-1"));
        assert_eq!(s.ctag.as_deref(), Some("c1"));
        assert_eq!(s.display_name.as_deref(), Some("Contacts"));
    }

    #[test]
    fn delta_applies_deletes_and_updates() {
        let cache = open_test_cache();
        cache
            .apply_contact_delta(
                "nc1",
                "contacts",
                None,
                &[row("u1", "Alice", "a@x.com")],
                &[],
                Some("t1"),
                None,
            )
            .unwrap();

        // Update Alice and delete u1's href in the same delta — no, that
        // contradicts; do them separately. Update first.
        cache
            .apply_contact_delta(
                "nc1",
                "contacts",
                None,
                &[ContactRow {
                    display_name: "Alice Updated".into(),
                    ..row("u1", "Alice", "a@x.com")
                }],
                &[],
                Some("t2"),
                None,
            )
            .unwrap();

        let after_update = cache.list_contacts(Some("nc1")).unwrap();
        assert_eq!(after_update[0].display_name, "Alice Updated");

        // Now delete by the href the row was stored at.
        cache
            .apply_contact_delta(
                "nc1",
                "contacts",
                None,
                &[],
                &["/dav/u1.vcf".into()],
                Some("t3"),
                None,
            )
            .unwrap();

        assert_eq!(cache.count_contacts("nc1").unwrap(), 0);
    }

    #[test]
    fn wipe_clears_sync_state_too() {
        let cache = open_test_cache();
        cache
            .apply_contact_delta(
                "nc1",
                "contacts",
                None,
                &[row("u1", "x", "x@x.com")],
                &[],
                Some("t"),
                None,
            )
            .unwrap();
        cache.wipe_nextcloud_contacts("nc1").unwrap();
        assert_eq!(cache.count_contacts("nc1").unwrap(), 0);
        assert!(
            cache
                .get_addressbook_sync_state("nc1", "contacts")
                .unwrap()
                .is_none()
        );
    }
}

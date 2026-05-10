//! Database schema and migrations for the local mail cache.
//!
//! # How migrations work
//!
//! The `MIGRATIONS` array is an ordered list of SQL blocks. Each entry is
//! one "version" of the schema; applying entry `N` moves the database from
//! version `N` to version `N+1`. The `schema_version` table stores the
//! current version as a single integer row.
//!
//! On startup we read `schema_version`, then run every migration whose
//! index is `>=` the current version. Each migration runs inside a
//! transaction so a failure leaves the DB untouched.
//!
//! # Why hand-rolled instead of a crate
//!
//! A project this size has a handful of migrations over its lifetime.
//! A crate like `refinery` or `rusqlite_migration` is fine, but brings
//! its own opinions; keeping the list right here is easier to reason
//! about while we're still exploring the schema. We can swap to a
//! migration crate later without disruption.
//!
//! # Adding a new migration
//!
//! **Only ever append** — never edit an existing entry, since users will
//! have the old one applied. Bump the schema by pushing another `&str`
//! onto `MIGRATIONS`. Keep each migration self-contained (all statements
//! needed to move from version N → N+1).

use rusqlite::Connection;

use crate::cache::CacheError;

/// Ordered migration scripts. Index `i` migrates schema version `i` → `i+1`.
///
/// The initial migration sets up the whole cache schema:
///
/// - `folders`: one row per mailbox per account (mirrors IMAP LIST output).
///   Primary key is `(account_id, name)` so a folder name is unique per
///   account but two accounts can both have an "INBOX".
///
/// - `messages`: envelope-level metadata — the light-weight fields shown
///   in the mail list. `uid` alone is not unique across folders (IMAP UIDs
///   are scoped per folder), so the natural key is `(account_id, folder, uid)`.
///   `internal_date` is indexed descending because the mail list view sorts
///   newest-first, which is the hot query path.
///
/// - `message_bodies`: the heavy fields (plain text, HTML, size) kept in a
///   separate table so envelope scans never drag MIME blobs through the
///   page cache. 1:1 with `messages`, same composite key, `ON DELETE CASCADE`.
///
/// - `folder_sync_state`: per-folder IMAP sync bookmarks. `uidvalidity` from
///   the server tells us whether our cached UIDs are still valid — if the
///   server returns a new value, everything for that folder must be wiped
///   and re-fetched. `highest_uid_seen` lets us do incremental syncs with
///   `UID FETCH highest+1:*`.
const MIGRATIONS: &[&str] = &[
    // ─────────────────────────────────────────────────────────────
    // v0 → v1: initial schema
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE folders (
        account_id     TEXT NOT NULL,
        name           TEXT NOT NULL,
        delimiter      TEXT,
        attributes     TEXT NOT NULL DEFAULT '[]',  -- JSON array of IMAP flags
        unread_count   INTEGER,
        PRIMARY KEY (account_id, name)
    );

    CREATE TABLE messages (
        account_id     TEXT    NOT NULL,
        folder         TEXT    NOT NULL,
        uid            INTEGER NOT NULL,
        from_addr      TEXT    NOT NULL DEFAULT '',
        subject        TEXT    NOT NULL DEFAULT '',
        internal_date  INTEGER NOT NULL,  -- unix epoch seconds
        is_read        INTEGER NOT NULL DEFAULT 0,
        is_starred     INTEGER NOT NULL DEFAULT 0,
        cached_at      INTEGER NOT NULL,  -- unix epoch seconds
        PRIMARY KEY (account_id, folder, uid)
    );

    -- Hot path: "newest 50 in this folder" — composite index ordered
    -- descending on internal_date so SQLite can satisfy the query
    -- with an index scan, no sort needed.
    CREATE INDEX messages_by_folder_date
        ON messages (account_id, folder, internal_date DESC);

    CREATE TABLE message_bodies (
        account_id       TEXT    NOT NULL,
        folder           TEXT    NOT NULL,
        uid              INTEGER NOT NULL,
        body_text        TEXT,
        body_html        TEXT,
        has_attachments  INTEGER NOT NULL DEFAULT 0,
        raw_size         INTEGER,
        cached_at        INTEGER NOT NULL,
        PRIMARY KEY (account_id, folder, uid),
        FOREIGN KEY (account_id, folder, uid)
            REFERENCES messages (account_id, folder, uid)
            ON DELETE CASCADE
    );

    CREATE TABLE folder_sync_state (
        account_id        TEXT    NOT NULL,
        folder            TEXT    NOT NULL,
        uidvalidity       INTEGER,
        highest_uid_seen  INTEGER,
        last_synced_at    INTEGER,
        PRIMARY KEY (account_id, folder)
    );
    "#,
    // ─────────────────────────────────────────────────────────────
    // v1 → v2: cache recipient headers so MailView can render from
    // the cache alone (no network round-trip on reopen).
    //
    // Stored as JSON-encoded arrays. IMAP address lists can get
    // genuinely weird (groups, nested comments, encoded words) so
    // a text blob is safer than trying to model rows per address —
    // and recipients are a display-only field for now, never
    // queried on.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE message_bodies ADD COLUMN to_addrs TEXT NOT NULL DEFAULT '[]';
    ALTER TABLE message_bodies ADD COLUMN cc_addrs TEXT NOT NULL DEFAULT '[]';
    "#,
    // ─────────────────────────────────────────────────────────────
    // v2 → v3: CardDAV contacts cache.
    //
    // - `contacts`: one row per vCard. Keyed by app-side `id`
    //   (`{nc_id}::{vcard_uid}`) so the UI has a single string handle,
    //   plus the natural `(nextcloud_account_id, addressbook, vcard_uid)`
    //   triple as a UNIQUE constraint to keep imports idempotent.
    //   `vcard_raw` is kept so we can re-extract fields if the model
    //   evolves without re-syncing every contact from the server.
    //
    // - `addressbook_sync_state`: per-collection bookmark for RFC 6578
    //   sync-collection. `sync_token` is what the server gave us last;
    //   we send it back to ask "what changed since". `ctag` is the
    //   pre-RFC-6578 cheap-check for "did anything change at all" —
    //   Nextcloud exposes both, we use ctag as the early-out and the
    //   sync token to enumerate the actual delta.
    //
    // Indexes:
    //   - display_name COLLATE NOCASE for the autocomplete LIKE scan
    //   - (nc_id, addressbook) for the per-addressbook sync upserts
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE contacts (
        id                    TEXT PRIMARY KEY,
        nextcloud_account_id  TEXT NOT NULL,
        addressbook           TEXT NOT NULL,
        vcard_uid             TEXT NOT NULL,
        href                  TEXT NOT NULL,
        etag                  TEXT NOT NULL,
        display_name          TEXT NOT NULL DEFAULT '',
        emails_json           TEXT NOT NULL DEFAULT '[]',
        phones_json           TEXT NOT NULL DEFAULT '[]',
        organization          TEXT,
        photo_mime            TEXT,
        photo_data            BLOB,
        vcard_raw             TEXT NOT NULL,
        cached_at             INTEGER NOT NULL,
        UNIQUE (nextcloud_account_id, addressbook, vcard_uid)
    );

    CREATE INDEX contacts_by_display_name
        ON contacts (display_name COLLATE NOCASE);

    CREATE INDEX contacts_by_addressbook
        ON contacts (nextcloud_account_id, addressbook);

    CREATE TABLE addressbook_sync_state (
        nextcloud_account_id  TEXT NOT NULL,
        addressbook           TEXT NOT NULL,
        display_name          TEXT,
        sync_token            TEXT,
        ctag                  TEXT,
        last_synced_at        INTEGER,
        PRIMARY KEY (nextcloud_account_id, addressbook)
    );
    "#,
    // ─────────────────────────────────────────────────────────────
    // v3 → v4: full-text search index for emails (Issue #15).
    //
    // FTS5 virtual table acting as a *contentless external-content*
    // index over `messages` joined with `message_bodies`. We use the
    // `content=''` (contentless) form and write index rows explicitly
    // via triggers so the indexed columns can come from two tables.
    //
    // Tokenizer:
    //   - `unicode61`  — Unicode-aware word splitter, handles UTF-8
    //     correctly for international names and subjects.
    //   - `remove_diacritics 2` — matches "müller" when searching
    //     "muller" (the common mail-client behaviour).
    //   - `porter` over unicode61 — stems English word endings so
    //     "invoices" matches "invoice". For non-English mail this
    //     is a no-op, which is fine.
    //
    // `rowid` is a synthetic row index — we keep an inverse lookup
    // via (account_id, folder, uid) stored on the row so we can map
    // FTS hits back to real messages.
    //
    // Triggers keep the index in lockstep with `messages` /
    // `message_bodies` so the app never has to remember to re-index.
    // Because FTS5 rows reference message data we guard deletes too.
    //
    // `search_meta` holds the lookup triple for each rowid — FTS5's
    // own rowid is the join key. We use INTEGER PRIMARY KEY so that
    // INSERT returns a stable autoincrementing rowid we can feed to
    // the FTS5 index.
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE search_meta (
        rowid        INTEGER PRIMARY KEY AUTOINCREMENT,
        account_id   TEXT NOT NULL,
        folder       TEXT NOT NULL,
        uid          INTEGER NOT NULL,
        UNIQUE (account_id, folder, uid)
    );

    CREATE VIRTUAL TABLE search_index USING fts5(
        subject,
        from_addr,
        to_addrs,
        cc_addrs,
        body,
        tokenize = 'porter unicode61 remove_diacritics 2'
    );

    -- Keep search_meta row in sync with messages lifecycle.
    CREATE TRIGGER search_meta_insert
    AFTER INSERT ON messages
    BEGIN
        INSERT OR IGNORE INTO search_meta (account_id, folder, uid)
        VALUES (NEW.account_id, NEW.folder, NEW.uid);
    END;

    CREATE TRIGGER search_meta_delete
    AFTER DELETE ON messages
    BEGIN
        DELETE FROM search_index
        WHERE rowid = (
            SELECT rowid FROM search_meta
            WHERE account_id = OLD.account_id
              AND folder = OLD.folder
              AND uid = OLD.uid
        );
        DELETE FROM search_meta
        WHERE account_id = OLD.account_id
          AND folder = OLD.folder
          AND uid = OLD.uid;
    END;

    -- Index the envelope fields as soon as the message row lands.
    -- Body columns are empty until a message_bodies row joins.
    CREATE TRIGGER search_index_envelope_insert
    AFTER INSERT ON messages
    BEGIN
        INSERT INTO search_index (rowid, subject, from_addr, to_addrs, cc_addrs, body)
        VALUES (
            (SELECT rowid FROM search_meta
             WHERE account_id = NEW.account_id
               AND folder = NEW.folder
               AND uid = NEW.uid),
            NEW.subject, NEW.from_addr, '', '', ''
        );
    END;

    CREATE TRIGGER search_index_envelope_update
    AFTER UPDATE OF subject, from_addr ON messages
    BEGIN
        UPDATE search_index
        SET subject = NEW.subject,
            from_addr = NEW.from_addr
        WHERE rowid = (
            SELECT rowid FROM search_meta
            WHERE account_id = NEW.account_id
              AND folder = NEW.folder
              AND uid = NEW.uid
        );
    END;

    -- When the body lands (or gets refreshed) splice in the heavy
    -- columns. We intentionally concat plain text only; HTML would
    -- pollute the index with tag noise and full-text mail search
    -- conventionally ignores markup.
    CREATE TRIGGER search_index_body_upsert
    AFTER INSERT ON message_bodies
    BEGIN
        UPDATE search_index
        SET to_addrs = NEW.to_addrs,
            cc_addrs = NEW.cc_addrs,
            body     = COALESCE(NEW.body_text, '')
        WHERE rowid = (
            SELECT rowid FROM search_meta
            WHERE account_id = NEW.account_id
              AND folder = NEW.folder
              AND uid = NEW.uid
        );
    END;

    CREATE TRIGGER search_index_body_update
    AFTER UPDATE ON message_bodies
    BEGIN
        UPDATE search_index
        SET to_addrs = NEW.to_addrs,
            cc_addrs = NEW.cc_addrs,
            body     = COALESCE(NEW.body_text, '')
        WHERE rowid = (
            SELECT rowid FROM search_meta
            WHERE account_id = NEW.account_id
              AND folder = NEW.folder
              AND uid = NEW.uid
        );
    END;

    -- Backfill: index everything already cached from earlier versions.
    -- New installs start empty so this is a no-op.
    INSERT INTO search_meta (account_id, folder, uid)
    SELECT account_id, folder, uid FROM messages;

    INSERT INTO search_index (rowid, subject, from_addr, to_addrs, cc_addrs, body)
    SELECT
        sm.rowid,
        m.subject,
        m.from_addr,
        COALESCE(b.to_addrs, ''),
        COALESCE(b.cc_addrs, ''),
        COALESCE(b.body_text, '')
    FROM search_meta sm
    INNER JOIN messages m
        ON m.account_id = sm.account_id
        AND m.folder = sm.folder
        AND m.uid = sm.uid
    LEFT JOIN message_bodies b
        ON b.account_id = sm.account_id
        AND b.folder = sm.folder
        AND b.uid = sm.uid;
    "#,
    // ─────────────────────────────────────────────────────────────
    // v4 → v5: CalDAV calendars + events cache (Issue #47).
    //
    // - `calendars`: one row per remote calendar. Keyed by app-side
    //   `id` (`{nc_id}::{path}`) so events can reference a single
    //   stable string; the natural `(nextcloud_account_id, path)` is
    //   also UNIQUE so a server-side rename stays idempotent.
    //   `sync_token` lives here — it's the RFC 6578 bookmark that
    //   makes every app-restart's first sync an incremental delta
    //   instead of a full re-fetch. `ctag` is the cheaper-than-
    //   sync-collection "did anything change at all" pre-check.
    //
    // - `calendar_events`: one row per VEVENT. A single href on the
    //   server can carry a master plus recurrence-id overrides; each
    //   of those lands as its own row sharing `(calendar_id, uid)`
    //   but distinguished by `recurrence_id` (NULL for the master,
    //   epoch seconds for an override). FK to `calendars` with
    //   CASCADE so deleting a calendar wipes its events in one go.
    //   `ics_raw` is kept so future model changes (and the
    //   recurrence expander in `nimbus_caldav::expand`) can
    //   re-extract from the cached blob without re-syncing.
    //
    // Indexes:
    //   - `calendar_events_by_start` on `(calendar_id, start_utc)`
    //     so the sidebar "next N events in this window" query can be
    //     satisfied by a single index range scan, no sort.
    //   - `calendar_events_by_href` on `(calendar_id, href)` so
    //     sync-collection deletes (which come as href lists) are O(1).
    //   - `calendars_by_nc_account` so "list calendars for this
    //     Nextcloud account" is a simple index seek.
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE calendars (
        id                    TEXT PRIMARY KEY,
        nextcloud_account_id  TEXT NOT NULL,
        path                  TEXT NOT NULL,
        display_name          TEXT NOT NULL DEFAULT '',
        color                 TEXT,
        ctag                  TEXT,
        sync_token            TEXT,
        last_synced_at        INTEGER,
        UNIQUE (nextcloud_account_id, path)
    );

    CREATE INDEX calendars_by_nc_account
        ON calendars (nextcloud_account_id);

    CREATE TABLE calendar_events (
        id             TEXT PRIMARY KEY,
        calendar_id    TEXT    NOT NULL,
        uid            TEXT    NOT NULL,
        href           TEXT    NOT NULL,
        etag           TEXT    NOT NULL,
        summary        TEXT    NOT NULL DEFAULT '',
        description    TEXT,
        start_utc      INTEGER NOT NULL,  -- unix epoch seconds
        end_utc        INTEGER NOT NULL,  -- unix epoch seconds
        location       TEXT,
        rrule          TEXT,
        rdate_json     TEXT    NOT NULL DEFAULT '[]',
        exdate_json    TEXT    NOT NULL DEFAULT '[]',
        -- NULL for a master (or a non-recurring event); epoch seconds
        -- of the original occurrence start for a RECURRENCE-ID override.
        recurrence_id  INTEGER,
        ics_raw        TEXT    NOT NULL,
        cached_at      INTEGER NOT NULL,
        FOREIGN KEY (calendar_id)
            REFERENCES calendars (id)
            ON DELETE CASCADE
    );

    CREATE INDEX calendar_events_by_start
        ON calendar_events (calendar_id, start_utc);

    CREATE INDEX calendar_events_by_href
        ON calendar_events (calendar_id, href);
    "#,
    // ─────────────────────────────────────────────────────────────
    // v5 → v6: cache attachment metadata for received messages.
    //
    // Before this migration the cache only remembered "does this
    // message have attachments?" as a bool. That's fine for the mail
    // list paperclip icon, but MailView now renders a proper
    // attachment list with filename / size / mime — which needs one
    // record per attachment.
    //
    // Shape: JSON-encoded `Vec<EmailAttachment>` on the
    // `message_bodies` row. We go with a blob column rather than a
    // separate table because:
    //   - Attachments never leave their message; there's no need to
    //     query across them or join from elsewhere.
    //   - We already treat `to_addrs` / `cc_addrs` the same way, so
    //     the pattern is established.
    //   - A cached `Email` deserialises straight back by feeding the
    //     text through `serde_json` — no per-attachment rehydration.
    //
    // NOT NULL with a '[]' default so older rows (written before this
    // column existed) decode to an empty list. That lines up with
    // `has_attachments = 0` on those rows.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE message_bodies
        ADD COLUMN attachments TEXT NOT NULL DEFAULT '[]';
    "#,
    // ─────────────────────────────────────────────────────────────
    // v6 → v7: extra calendar-event fields the editor exposes
    // (Issue #50).
    //
    // Adds the four fields that the new "all fields" editor edits but
    // the original sync schema only kept inside `ics_raw`:
    //
    //   - `url`            — VEVENT `URL` property.
    //   - `transparency`   — `TRANSP`, the busy/free flag.
    //   - `attendees_json` — `Vec<EventAttendee>` (CN + email + status).
    //   - `reminders_json` — `Vec<EventReminder>` (one row per VALARM).
    //
    // We could re-parse them out of `ics_raw` on every read, but the
    // expansion path runs on every UI repaint and re-parsing would
    // burn cycles for no benefit. JSON columns match the existing
    // pattern (`rdate_json`, `exdate_json`, `attachments`) and let
    // `serde_json` round-trip the whole `Vec<…>` in one call.
    //
    // NOT NULL with sensible defaults so older rows (written before
    // this column existed) decode without a separate backfill: empty
    // arrays for the lists and NULL for the singletons.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE calendar_events
        ADD COLUMN url TEXT;
    ALTER TABLE calendar_events
        ADD COLUMN transparency TEXT;
    ALTER TABLE calendar_events
        ADD COLUMN attendees_json TEXT NOT NULL DEFAULT '[]';
    ALTER TABLE calendar_events
        ADD COLUMN reminders_json TEXT NOT NULL DEFAULT '[]';
    "#,
    // ─────────────────────────────────────────────────────────────
    // v7 → v8: move email accounts from `accounts.json` into the
    // encrypted SQLite cache (Issue #60).
    //
    // Why: accounts.json sits next to the database in the user's
    // config dir as plaintext. Moving it inside SQLCipher gives us
    // at-rest encryption for the whole account record (host names,
    // signatures, the lot) without a separate keychain entry per
    // field. It also opens the door to foreign keys from `messages`
    // onto an `account_id` once we want cascade-on-delete semantics.
    //
    // Schema mirrors the `Account` struct one-to-one. Lists / option
    // types that don't fit a column (`folder_icons`,
    // `trusted_fingerprints` once #60 lands TLS trust) are kept as
    // JSON blobs — same pattern we use elsewhere (`rdate_json`,
    // `attendees_json`, …) and lets `serde_json` round-trip the
    // whole field in one call.
    //
    // Migration of existing data is *not* part of this DDL — it
    // happens lazily in `account_store::load_accounts` on the first
    // call after the upgrade. That keeps the migration code owned
    // by the same module that knows about the JSON file format.
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE accounts (
        id                TEXT PRIMARY KEY,
        display_name      TEXT NOT NULL,
        email             TEXT NOT NULL,
        imap_host         TEXT NOT NULL,
        imap_port         INTEGER NOT NULL,
        smtp_host         TEXT NOT NULL,
        smtp_port         INTEGER NOT NULL,
        use_jmap          INTEGER NOT NULL DEFAULT 0,
        jmap_url          TEXT,
        signature         TEXT,
        folder_icons_json TEXT NOT NULL DEFAULT '[]',
        -- Insertion order is the natural sort for the account
        -- switcher; SQLite assigns rowids monotonically so we read
        -- back with `ORDER BY rowid`.
        created_at        INTEGER NOT NULL
    );
    "#,
    // ─────────────────────────────────────────────────────────────
    // v8 → v9: per-account TLS trust list (Issue #60).
    //
    // When the user knowingly accepts a self-signed cert during
    // account setup, we stash the cert's DER bytes here so every
    // future IMAP/SMTP connect from that account can plug it into
    // the rustls root store. The list is a JSON array of
    // `TrustedCert` records (DER, sha256 fingerprint, host, added
    // timestamp) — same JSON-blob pattern the rest of the schema
    // uses for variable-length structured fields.
    //
    // NOT NULL with `'[]'` default so older account rows decode
    // without a backfill: an empty list means "trust webpki-roots
    // only", which is the historical behaviour.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE accounts
        ADD COLUMN trusted_certs_json TEXT NOT NULL DEFAULT '[]';
    "#,
    // ─────────────────────────────────────────────────────────────
    // v9 → v10: extra contact fields (Issue #66).
    //
    // Adds the fields the new contact-card view exposes — title,
    // birthday, addresses, urls, note. We store the variable-length
    // ones (addresses, urls) as JSON blobs alongside the existing
    // `emails_json` / `phones_json` columns; the singletons get
    // their own scalar columns.
    //
    // NOT NULL with sensible empty defaults so older contact rows
    // (written before this column existed) decode straight back to
    // an empty list / NULL singleton without a separate backfill.
    // The `vcard_raw` blob still carries the source data so a future
    // re-parse can pull anything we missed.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE contacts ADD COLUMN title TEXT;
    ALTER TABLE contacts ADD COLUMN birthday TEXT;
    ALTER TABLE contacts ADD COLUMN note TEXT;
    ALTER TABLE contacts ADD COLUMN addresses_json TEXT NOT NULL DEFAULT '[]';
    ALTER TABLE contacts ADD COLUMN urls_json TEXT NOT NULL DEFAULT '[]';
    "#,
    // ─────────────────────────────────────────────────────────────
    // v10 → v11: per-folder icon overrides on the account record.
    //
    // Backs the Sidebar's right-click → Change icon flow. Stored as
    // a JSON blob keyed by the full folder path (same convention as
    // `folder_icons_json` — the keyword-rule list that predates
    // this) so nested paths like `INBOX/Projects/2026` don't
    // collide with sibling folders that happen to share a leaf
    // name. Empty map is the historical behaviour — every folder
    // falls through to special-use detection + keyword rules + 📁.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE accounts
        ADD COLUMN folder_icon_overrides_json TEXT NOT NULL DEFAULT '{}';
    "#,
    // ─────────────────────────────────────────────────────────────
    // v11 → v12: per-calendar visibility toggle (Issue #82).
    //
    // Local-only state — never synced to the server. Drives the
    // "hide this calendar from the sidebar" checkboxes in
    // NextcloudSettings and the `hidden` filter in CalendarView.
    // Default 0 (visible) so existing calendars roll forward
    // unchanged; the toggle is opt-in per calendar.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE calendars
        ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
    "#,
    // ─────────────────────────────────────────────────────────────
    // v12 → v13: per-calendar mute toggle (two-layer visibility).
    //
    // `hidden` (Layer 1, Settings) removes a calendar from the
    // sidebar entirely. `muted` (Layer 2, sidebar swatch) keeps the
    // calendar listed in the sidebar but stops its events from
    // painting on the grid. Also local-only — never synced to the
    // server. Default 0 so existing calendars are fully visible.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE calendars
        ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;
    "#,
    // ─────────────────────────────────────────────────────────────
    // v13 → v14: persisted iMIP RSVP responses (#58).
    //
    // When the user clicks Accept / Decline / Tentative on an
    // inbound invite, we send a REPLY mail and remember the chosen
    // PARTSTAT keyed by the event's iCalendar UID. Reopening the
    // invite later (different folder, app restart, account switch)
    // shows the post-reply state instead of the fresh "Accept /
    // Decline" buttons.
    //
    // UID is the natural key — globally unique per RFC 5545 — and
    // is what pairs the inbound REQUEST with whichever REPLY we
    // sent for it. A later RSVP overwrites the previous row, so
    // changing the answer just updates `partstat` + `responded_at`.
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE IF NOT EXISTS rsvp_responses (
        uid           TEXT PRIMARY KEY,
        partstat      TEXT NOT NULL,
        responded_at  INTEGER NOT NULL
    );
    "#,
    // ─────────────────────────────────────────────────────────────
    // v14 → v15: cancelled-invite registry.
    //
    // When MailView opens a `METHOD:CANCEL` iMIP message, we
    // persist its iCalendar UID here.  The inbox RSVP card's
    // pre-render sync checks this table and flips the original
    // `METHOD:REQUEST` mail's card to the cancelled banner —
    // stops the user from accidentally answering an invite
    // whose meeting has since been cancelled.
    //
    // Keyed by UID since the same meeting can have a REQUEST
    // mail and a CANCEL mail in different folders / accounts;
    // both should reflect the cancellation.
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE IF NOT EXISTS cancelled_invites (
        uid           TEXT PRIMARY KEY,
        cancelled_at  INTEGER NOT NULL
    );
    "#,
    // ─────────────────────────────────────────────────────────────
    // v15 → v16: account UI polish (Issue #115).
    //
    // - `emoji` — optional avatar emoji shown in the IconRail in
    //   place of the initials bubble.  NULL falls back to initials.
    // - `sort_order` — display rank in the IconRail / Settings.
    //   Lower values render first; ties break on `id`.  Defaults to
    //   `0` so existing rows keep their insertion order until the
    //   user reorders.
    // - `person_name` — human's full name for the From: header,
    //   separate from `display_name` (the account label).  NULL
    //   falls back to `display_name`, preserving pre-#115 behaviour.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE accounts ADD COLUMN emoji TEXT;
    ALTER TABLE accounts ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE accounts ADD COLUMN person_name TEXT;
    "#,
    // ─────────────────────────────────────────────────────────────
    // v16 → v17: contact groups / mailing lists (#133, #113).
    //
    // Groups live as plain vCards with `KIND:group` and `MEMBER:`
    // properties — we just need to flag them in the cache and
    // carry the member UID list alongside the regular contact
    // fields.  `group_emoji` and `group_hidden` are local-only
    // (no vCard equivalent) and only meaningful for rows where
    // `kind = 'group'`; we keep them on every contact row to
    // avoid a separate table.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE contacts ADD COLUMN kind TEXT NOT NULL DEFAULT '';
    ALTER TABLE contacts ADD COLUMN member_uids_json TEXT NOT NULL DEFAULT '[]';
    ALTER TABLE contacts ADD COLUMN group_emoji TEXT;
    ALTER TABLE contacts ADD COLUMN group_hidden INTEGER NOT NULL DEFAULT 0;
    "#,
    // ─────────────────────────────────────────────────────────────
    // v17 → v18: CATEGORIES (Kontaktgruppen) on contacts (#133
    // redesign).  Comma-separated tags on each vCard — what NC's
    // Contacts UI calls "Kontaktgruppen" and what iOS shows as
    // Groups.  Stored as JSON so adding/removing one tag stays
    // a single column rewrite.
    //
    // Plus a `mailing_list_settings` table for per-row local
    // overlays (the per-row hide-from-autocomplete swatch on
    // the new Mailing Lists tab).  Keyed by the unified
    // mailing-list id (`cat:<name>` / `group:<id>` /
    // `team:<id>` / `list:<vcard-uid>`) so all four sources
    // share one settings table.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE contacts ADD COLUMN categories_json TEXT NOT NULL DEFAULT '[]';
    CREATE TABLE IF NOT EXISTS mailing_list_settings (
        id                       TEXT PRIMARY KEY,
        hidden_from_autocomplete INTEGER NOT NULL DEFAULT 0
    );
    "#,
    // ─────────────────────────────────────────────────────────────
    // v18 → v19: per-list emoji overlay (#133 follow-up).  Local-
    // only avatar override shown in place of the source icon
    // (🏷️/📨/⚡) on the Mailing Lists tab.  NULL falls back to
    // the source icon.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE mailing_list_settings ADD COLUMN emoji TEXT;
    "#,
    // ─────────────────────────────────────────────────────────────
    // v19 → v20: attachment-thumbnail cache (#157).
    //
    // MailView used to re-fetch every image / video attachment's
    // bytes on each open to build a 36×36 thumbnail strip.  For
    // a video that means a fresh GStreamer pipeline + canvas
    // extraction each visit; for a 5 MiB photo it's an IPC round-
    // trip + Blob construction per render.  Persist the
    // generated thumbnail (JPEG-encoded, ≤256 px on the long
    // edge) per (account, folder, uid, part_id) so repeat opens
    // skip the work entirely.
    //
    // Keyed on the same tuple MailView already addresses
    // attachments by; cascades-on-delete are not strictly
    // necessary because the IMAP UID is reused only after
    // EXPUNGE and a new fetch repopulates the row, but we lean
    // on a foreign key from `message_bodies(account_id,
    // folder, uid)` so that a message's removal also evicts
    // its previews and we don't accumulate orphans.
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE IF NOT EXISTS attachment_previews (
        account_id TEXT    NOT NULL,
        folder     TEXT    NOT NULL,
        uid        INTEGER NOT NULL,
        part_id    INTEGER NOT NULL,
        mime       TEXT    NOT NULL,
        bytes      BLOB    NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (account_id, folder, uid, part_id)
    );
    CREATE INDEX IF NOT EXISTS idx_attachment_previews_msg
        ON attachment_previews(account_id, folder, uid);
    "#,
    // ─────────────────────────────────────────────────────────────
    // v20 → v21: move Nextcloud connections from
    // `<config-dir>/nimbus-mail/nextcloud_accounts.json` into the
    // encrypted cache (#155).  Mirrors what #60 did for the mail
    // accounts table.
    //
    // - `id` is the stable UUID also used as the keychain account
    //   key for the app password.  Keychain entry is unchanged.
    // - `capabilities_json` is the `NextcloudCapabilities` struct
    //   serialised as JSON.  We never query into it from SQL —
    //   the UI deserialises the whole thing — so a single TEXT
    //   column is simpler than splitting each cap into its own
    //   column and re-serialising.
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE IF NOT EXISTS nextcloud_accounts (
        id                 TEXT PRIMARY KEY,
        server_url         TEXT NOT NULL,
        username           TEXT NOT NULL,
        display_name       TEXT,
        capabilities_json  TEXT
    );
    "#,
    // ─────────────────────────────────────────────────────────────
    // v21 → v22: optimistic-action tombstone column (#174).
    //
    // The mail-triage IPCs (`delete_message`, `move_message`,
    // `move_messages`) used to await the IMAP roundtrip before
    // returning, so the UI couldn't drop the row until the server
    // confirmed.  We now want optimistic UI: the row disappears
    // immediately and the IMAP work runs in the background.
    //
    // To keep the optimistic state consistent across folder
    // switches (where a fresh `get_envelopes` would otherwise
    // resurrect the row from the cache), each in-flight action
    // marks the cache row with a `pending_action` string before
    // the IMAP call.  Envelope-list queries filter out rows
    // where `pending_action IS NOT NULL`.  On IMAP success the
    // existing post-action cleanup already drops or moves the
    // row; on failure the IPC clears `pending_action` so the
    // row reappears in the next list pull.
    //
    // Values: `'delete'` for delete/move-to-trash, `'move:<dest>'`
    // for explicit folder moves.  Rolling our own enum-as-text
    // keeps the migration trivial — no new table, no FK pain.
    // ─────────────────────────────────────────────────────────────
    r#"
    ALTER TABLE messages ADD COLUMN pending_action TEXT;
    "#,
    // ─────────────────────────────────────────────────────────────
    // vN → vN+1: URLhaus link-safety table (#165)
    //
    // Local snapshot of abuse.ch's URLhaus "online malicious URLs"
    // dump.  Refreshed every ~hour by a background task; the
    // mail-render path looks up each <a href> against this table
    // and renders a Safe / Unsafe pill next to the link.
    //
    // We store both the full URL and a derived `host` so we can
    // ask either question (exact match → "this link is on
    // URLhaus", or just-host match → "this domain has hosted
    // malware before").  v1 only renders the binary safe/unsafe
    // pill, but we keep the host index in case a future tier of
    // "caution" is wanted without another migration.
    //
    // `meta` is the small key/value table for sync state — last
    // refresh timestamp, source ETag, etc.  One row per key.
    // ─────────────────────────────────────────────────────────────
    r#"
    CREATE TABLE urlhaus_urls (
        url            TEXT PRIMARY KEY,
        host           TEXT NOT NULL,
        threat         TEXT NOT NULL DEFAULT '',
        tags           TEXT NOT NULL DEFAULT '',  -- comma-separated tag list
        date_added     INTEGER NOT NULL,           -- unix epoch seconds
        last_refreshed INTEGER NOT NULL            -- unix epoch seconds
    );

    CREATE INDEX urlhaus_by_host ON urlhaus_urls (host);

    CREATE TABLE urlhaus_meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // ─────────────────────────────────────────────────────────────
    // v23 → v24: Nextcloud Notes cache (#138)
    //
    // The Notes REST app exposes flat documents keyed by an integer
    // `id`, with `etag` for optimistic concurrency and `modified` as
    // a Unix timestamp.  We cache them locally so the list paints
    // instantly and edits work offline.  `category` is a vCard-
    // style path string that NC interprets as nested folders
    // (e.g. `Joplin/ProjectX` renders as a sub-folder); we store
    // it verbatim and let the UI tree-build at render time.
    //
    // `notes_sync_state` carries the high-water marks the
    // background sync uses to skip unchanged documents; it's
    // intentionally separate from the main notes table so a forced
    // re-sync only has to clear one row per account.
    r#"
    CREATE TABLE notes (
        nextcloud_account_id  TEXT NOT NULL,
        note_id               INTEGER NOT NULL,
        etag                  TEXT NOT NULL,
        modified_unix         INTEGER NOT NULL,
        title                 TEXT NOT NULL DEFAULT '',
        category              TEXT NOT NULL DEFAULT '',
        content               TEXT NOT NULL DEFAULT '',
        favorite              INTEGER NOT NULL DEFAULT 0,
        cached_at             INTEGER NOT NULL,
        PRIMARY KEY (nextcloud_account_id, note_id)
    );

    CREATE INDEX notes_by_modified
        ON notes (nextcloud_account_id, modified_unix DESC);

    CREATE INDEX notes_by_category
        ON notes (nextcloud_account_id, category);

    CREATE TABLE notes_sync_state (
        nextcloud_account_id  TEXT PRIMARY KEY,
        last_synced_at        INTEGER
    );
    "#,
    // ─────────────────────────────────────────────────────────────
    // v24 → v25: per-Nextcloud-account self-signed cert trust list
    // (#253).  Mirrors `accounts.trusted_certs_json` for IMAP/SMTP
    // — JSON-serialised `Vec<TrustedCert>` so the same fingerprint
    // verifier in `nimbus-core::tls` covers Nextcloud HTTPS too
    // (OCS, CalDAV, CardDAV, Notes, Talk, Files).  Defaults to the
    // empty array so existing accounts deserialise cleanly.
    r#"
    ALTER TABLE nextcloud_accounts
        ADD COLUMN trusted_certs_json TEXT NOT NULL DEFAULT '[]';
    "#,
    // ─────────────────────────────────────────────────────────────
    // v25 → v26: track which messages the user has answered so the
    // mail list can show a small reply / reply-all / meeting-reply
    // icon in front of the subject (#255).
    //
    // Two columns:
    //   * `is_answered` — mirrors the IMAP `\Answered` system flag.
    //     Refreshed on every envelope re-fetch.  `1` is enough to
    //     show a generic reply icon for messages the user
    //     answered before this feature shipped (or answered from
    //     a different client) — IMAP-canonical fallback.
    //   * `replied_kind` — Nimbus-only metadata recording *how*
    //     the user replied: `'reply'`, `'reply-all'`, or
    //     `'meeting'`.  IMAP carries one boolean answered bit;
    //     the kind is something only we know, so we store it
    //     locally and never overwrite it on envelope re-fetches.
    //     `NULL` means "we didn't reply via Nimbus" — fall back
    //     to `is_answered` for the icon decision.
    r#"
    ALTER TABLE messages
        ADD COLUMN is_answered INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages
        ADD COLUMN replied_kind TEXT;
    "#,
    // ─────────────────────────────────────────────────────────────
    // v26 → v27: local-only Outbox folder (#276).
    //
    // Every send routes through this table first.  On a healthy
    // network the row is created and removed within the same tick
    // (the send-driven drain task handles it sub-second), so the
    // user never sees the synthetic "Outbox" folder appear.  When
    // SMTP fails (offline, timeout, server refusal), the row stays
    // and the periodic `background_sync_loop` retry sweep keeps
    // attempting on every sync tick until success or the user
    // manually deletes / edits.
    //
    // Stored fields:
    //   * `outgoing_json` — full `OutgoingEmail` for both edit
    //     (re-open in Compose) and retry (rebuild lettre Message).
    //   * `replied_to_json` — optional `RepliedToRef` so a
    //     successful retry still flips the IMAP `\Answered` flag
    //     on the original message (#255 follow-up).
    //   * `from_header` / `to_display` / `subject` — pre-computed
    //     display fields so the Outbox list view renders without
    //     re-parsing the JSON for every row.
    //   * `attempt_count` / `last_attempt_at` / `last_error` — UI
    //     status, surfaces "Why is this stuck?" inline on the row.
    //   * `skip_sent_copy` — preserved through retries so calendar
    //     machinery (RSVP REPLY, grid invites) still skips the
    //     IMAP APPEND-to-Sent on success, same as today.
    r#"
    CREATE TABLE outbox_messages (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        account_id      TEXT NOT NULL,
        outgoing_json   TEXT NOT NULL,
        replied_to_json TEXT,
        from_header     TEXT NOT NULL DEFAULT '',
        to_display      TEXT NOT NULL DEFAULT '',
        subject         TEXT NOT NULL DEFAULT '',
        queued_at       INTEGER NOT NULL,
        attempt_count   INTEGER NOT NULL DEFAULT 0,
        last_attempt_at INTEGER,
        last_error      TEXT,
        skip_sent_copy  INTEGER NOT NULL DEFAULT 0
    );

    CREATE INDEX outbox_by_account
        ON outbox_messages (account_id, queued_at DESC);
    "#,
    // ─────────────────────────────────────────────────────────────
    // v27 → v28: per-calendar read-only flag (#236).
    //
    // Reflects the CalDAV `current-user-privilege-set` PROPFIND
    // result.  `1` means the user only has read access
    // (typical for shared calendars where the owner granted
    // view-only access), so the EventEditor hides the Delete
    // button and removes the calendar from the new-event picker.
    // Default `0` keeps existing rows writable until the next
    // discovery cycle stamps the actual value — preserves the
    // pre-#236 happy path for servers that don't advertise the
    // prop at all.
    r#"
    ALTER TABLE calendars
        ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
    "#,
    // v28 → v29: GEO lat/lon on calendar events (#280).
    //
    // RFC 5545 §3.8.1.6.  Populated by the EventEditor's
    // location-autocomplete pick so the inline map preview can
    // drop a pin on the canonical place; round-trips through
    // `text/calendar` so other CalDAV clients see the same
    // geocoded location.  `NULL` for events whose `LOCATION`
    // is plain text without a geocoded match — pre-#280 rows
    // start there and stay there until the user re-saves.
    r#"
    ALTER TABLE calendar_events
        ADD COLUMN latitude REAL;
    ALTER TABLE calendar_events
        ADD COLUMN longitude REAL;

    -- Local cache for Nominatim geocoding hits.  Hit by the
    -- LocationField autocomplete (#280) so the same query
    -- typed twice (or two events to the same address) doesn't
    -- spend two upstream API calls.  Keyed by the lower-cased
    -- query text + a canonical language code so a `?cafe`
    -- search and a separate `cafe ` search dedupe to one row.
    CREATE TABLE IF NOT EXISTS geocode_cache (
        query        TEXT NOT NULL,
        lang         TEXT NOT NULL DEFAULT '',
        results_json TEXT NOT NULL,
        cached_at    INTEGER NOT NULL,
        PRIMARY KEY (query, lang)
    );
    "#,
];

const SCHEMA_VERSION_SQL: &str = r#"
    CREATE TABLE IF NOT EXISTS schema_version (
        id      INTEGER PRIMARY KEY CHECK (id = 1),
        version INTEGER NOT NULL
    );
    INSERT OR IGNORE INTO schema_version (id, version) VALUES (1, 0);
"#;

/// Bring the database up to the latest schema version.
///
/// Runs every pending migration inside its own transaction, bumping the
/// recorded `schema_version` after each one. If a migration fails, the
/// transaction rolls back and we return the error — the DB stays at the
/// previous version rather than landing in a half-migrated state.
pub fn run_migrations(conn: &mut Connection) -> Result<(), CacheError> {
    // Ensure the version table exists before we try to read from it.
    conn.execute_batch(SCHEMA_VERSION_SQL)
        .map_err(|e| CacheError::Migration(format!("failed to init schema_version: {e}")))?;

    let current: i64 = conn
        .query_row("SELECT version FROM schema_version WHERE id = 1", [], |r| {
            r.get(0)
        })
        .map_err(|e| CacheError::Migration(format!("failed to read schema_version: {e}")))?;

    let target = MIGRATIONS.len() as i64;
    if current == target {
        tracing::debug!("Cache schema already at version {current}");
        return Ok(());
    }

    tracing::info!("Migrating cache schema v{current} → v{target}");

    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn
            .transaction()
            .map_err(|e| CacheError::Migration(format!("begin tx: {e}")))?;

        tx.execute_batch(sql)
            .map_err(|e| CacheError::Migration(format!("migration v{} → v{}: {e}", i, i + 1)))?;

        tx.execute(
            "UPDATE schema_version SET version = ?1 WHERE id = 1",
            [(i + 1) as i64],
        )
        .map_err(|e| CacheError::Migration(format!("bump version: {e}")))?;

        tx.commit()
            .map_err(|e| CacheError::Migration(format!("commit tx: {e}")))?;

        tracing::debug!("Applied migration v{} → v{}", i, i + 1);
    }

    Ok(())
}

//! Full-text search over the cached mail corpus (Issue #15).
//!
//! # How this works
//!
//! The cache schema (see `schema.rs`, migration v3 → v4) carries an
//! FTS5 virtual table `search_index` with the columns `subject`,
//! `from_addr`, `to_addrs`, `cc_addrs`, and `body`. Triggers keep it
//! in sync with `messages` / `message_bodies`, so every write through
//! the normal upsert paths also lands in the index for free.
//!
//! This module is the *read* side: parse a user query into field
//! filters + a free-text match expression, run it, and return
//! enriched envelopes with highlighted snippets.
//!
//! # Query syntax
//!
//! Operator-prefixed query syntax (FROM:, TO:, SUBJECT:, etc.) — we
//! translate it to FTS5's native `column:term` expression and WHERE-
//! clause filters on the real columns. Supported forms:
//!
//! - `from:alice`        → match in `from_addr`
//! - `to:bob`            → match in `to_addrs`
//! - `subject:foo`       → match in `subject`
//! - `body:bar`          → match in `body`
//! - `"exact phrase"`    → phrase search, all indexed columns
//! - `has:attachment`    → filter `has_attachments = 1`
//! - `is:unread`         → filter `is_read = 0`
//! - `is:read`           → filter `is_read = 1`
//! - `is:flagged`        → filter `is_starred = 1`
//! - `after:2026-01-31`  → sent on or after that date (`since:` alias)
//! - `before:2026-01-31` → sent strictly before that date
//! - `on:2026-01-31`     → sent within that day
//! - `in:Sent`           → only that folder (`folder:` alias);
//!   `in:anywhere` widens a folder-scoped search back to all folders
//! - plain words         → match in any indexed column
//!
//! Date operands accept `YYYY-MM-DD`, `YYYY/MM/DD`, `DD.MM.YYYY`,
//! and the coarser `YYYY-MM` / `YYYY` (which mean the whole month /
//! year — `on:2026-03` is all of March). Dates are interpreted in
//! the user's local timezone, matching what the mail list displays.
//!
//! Operators combine with AND semantics. Everything after a known
//! operator up to the next whitespace is the operand, unless the
//! operand is a double-quoted phrase — then we keep going until the
//! closing quote.

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use rusqlite::{ToSql, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::cache::{Cache, CacheError};

/// Optional scope narrowing — the standard "current folder vs.
/// all folders" scope selector. `Current folder` is represented by
/// passing `Some(folder)`; "all folders across all accounts" is
/// the empty `SearchScope`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchScope {
    /// Limit to this account id, or all accounts if `None`.
    #[serde(default)]
    pub account_id: Option<String>,
    /// Limit to this folder, or all folders if `None`.
    #[serde(default)]
    pub folder: Option<String>,
    /// Maximum number of results. Defaults to 200 when zero / missing
    /// to keep a runaway query from choking the UI.
    #[serde(default)]
    pub limit: u32,
}

/// Additional explicit filters layered on top of the operator parse.
/// The UI can drive these as toggle chips without touching the query
/// string — keeps "has attachment" / "unread" one click away even when
/// the user is typing free text.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilters {
    #[serde(default)]
    pub unread_only: bool,
    #[serde(default)]
    pub flagged_only: bool,
    #[serde(default)]
    pub has_attachment: bool,
    /// Unix epoch seconds — inclusive lower bound.
    #[serde(default)]
    pub date_from: Option<i64>,
    /// Unix epoch seconds — inclusive upper bound.
    #[serde(default)]
    pub date_to: Option<i64>,
}

/// A single search hit. Carries enough fields to render a mail-list
/// row without a second lookup, plus a highlighted snippet for the
/// results pane.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub account_id: String,
    pub folder: String,
    pub uid: u32,
    pub from: String,
    pub subject: String,
    pub date: DateTime<Utc>,
    pub is_read: bool,
    pub is_starred: bool,
    pub has_attachments: bool,
    /// FTS5 `snippet()` output with `<mark>…</mark>` around matches.
    /// Empty string when the query is purely filter-based.
    pub snippet: String,
    /// Kebab-case `unkai_crypto::Protection` for the hit, same
    /// sourcing as the envelope read paths: `COALESCE(b.protection,
    /// m.protection)` (#440).  Consumers that surface snippets to
    /// third parties (the MCP mail tools) need this to know when a
    /// snippet was cut from decrypted E2E-encrypted plaintext —
    /// the FTS index stores whatever `message_bodies` holds, which
    /// for auto-unlock accounts is the decrypted text.  `None` =
    /// plain message or body not fetched yet.
    pub protection: Option<String>,
}

impl Cache {
    /// Run a search against the local FTS5 index.
    ///
    /// Returns hits newest-first. The query is parsed by
    /// [`parse_query`]; `scope` restricts to an account/folder and
    /// caps the result count; `filters` layers boolean toggles on
    /// top (UI chips). An empty query with only filters is valid and
    /// returns the newest matching messages — equivalent to a
    /// "Has attachments" sidebar filter on its own.
    pub fn search_emails(
        &self,
        query: &str,
        scope: &SearchScope,
        filters: &SearchFilters,
    ) -> Result<Vec<SearchHit>, CacheError> {
        let parsed = parse_query(query);
        let conn = self.conn()?;

        // Build SQL + bindings incrementally so we can toggle the
        // FTS5 join off when there's no text to match (pure filter
        // query) — saves a virtual-table scan.
        let mut sql = String::from(
            "SELECT m.account_id, m.folder, m.uid, m.from_addr, m.subject,
                    m.internal_date, m.is_read, m.is_starred,
                    COALESCE(b.has_attachments, 0) AS has_attachments,
                    COALESCE(b.protection, m.protection) AS protection,",
        );
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();

        if parsed.has_text() {
            // Snippet args: (table, col, start, end, ellipsis, tokens).
            // `col = -1` tells FTS5 "pick the best matching column".
            sql.push_str(" snippet(search_index, -1, '<mark>', '</mark>', '…', 16) AS snippet ");
            sql.push_str(
                "FROM search_index
                 INNER JOIN search_meta sm ON sm.rowid = search_index.rowid
                 INNER JOIN messages m
                   ON m.account_id = sm.account_id
                   AND m.folder = sm.folder
                   AND m.uid = sm.uid
                 LEFT JOIN message_bodies b
                   ON b.account_id = m.account_id
                   AND b.folder = m.folder
                   AND b.uid = m.uid
                 WHERE search_index MATCH ?
                   AND m.pending_action IS NULL",
            );
            params.push(Box::new(parsed.match_expression()));
        } else {
            sql.push_str(
                " '' AS snippet
                 FROM messages m
                 LEFT JOIN message_bodies b
                   ON b.account_id = m.account_id
                   AND b.folder = m.folder
                   AND b.uid = m.uid
                 WHERE 1 = 1
                   AND m.pending_action IS NULL",
            );
        }

        // Apply scope + filters. Parameters are appended in the same
        // order we push them into `params`.
        if let Some(acc) = &scope.account_id {
            sql.push_str(" AND m.account_id = ?");
            params.push(Box::new(acc.clone()));
        }
        // Folder: an explicit `in:`/`folder:` operator beats the UI
        // scope selector (typing it is the more deliberate act), and
        // `in:anywhere` drops the restriction entirely. The operator
        // path matches case-insensitively because users type `in:sent`
        // while IMAP reports `Sent`; the scope path stays exact since
        // it carries a folder name the app itself supplied.
        if let Some(f) = &parsed.folder {
            sql.push_str(" AND m.folder = ? COLLATE NOCASE");
            params.push(Box::new(f.clone()));
        } else if let Some(f) = &scope.folder {
            if !parsed.folder_any {
                sql.push_str(" AND m.folder = ?");
                params.push(Box::new(f.clone()));
            }
        }
        if filters.unread_only {
            sql.push_str(" AND m.is_read = 0");
        }
        if filters.flagged_only {
            sql.push_str(" AND m.is_starred = 1");
        }
        if filters.has_attachment {
            sql.push_str(" AND COALESCE(b.has_attachments, 0) = 1");
        }
        // Date bounds: UI-filter bounds and operator bounds (`after:` /
        // `before:` / `on:`) intersect — every source only ever narrows.
        let date_from = match (filters.date_from, parsed.date_from) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let date_to = match (filters.date_to, parsed.date_to) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(from) = date_from {
            sql.push_str(" AND m.internal_date >= ?");
            params.push(Box::new(from));
        }
        if let Some(to) = date_to {
            sql.push_str(" AND m.internal_date <= ?");
            params.push(Box::new(to));
        }

        // Fold operator-based filters from the parsed query.
        if parsed.is_unread == Some(true) {
            sql.push_str(" AND m.is_read = 0");
        } else if parsed.is_unread == Some(false) {
            sql.push_str(" AND m.is_read = 1");
        }
        if parsed.is_flagged == Some(true) {
            sql.push_str(" AND m.is_starred = 1");
        }
        if parsed.has_attachment == Some(true) {
            sql.push_str(" AND COALESCE(b.has_attachments, 0) = 1");
        }

        sql.push_str(" ORDER BY m.internal_date DESC LIMIT ?");
        let limit = if scope.limit == 0 { 200 } else { scope.limit };
        params.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn ToSql> = params.iter().map(|p| &**p).collect();
        let rows = stmt.query_map(params_from_iter(param_refs.iter()), |r| {
            let ts: i64 = r.get(5)?;
            let date = Utc.timestamp_opt(ts, 0).single().unwrap_or_else(Utc::now);
            Ok(SearchHit {
                account_id: r.get(0)?,
                folder: r.get(1)?,
                uid: r.get::<_, i64>(2)? as u32,
                from: r.get(3)?,
                subject: r.get(4)?,
                date,
                is_read: r.get::<_, i64>(6)? != 0,
                is_starred: r.get::<_, i64>(7)? != 0,
                has_attachments: r.get::<_, i64>(8)? != 0,
                protection: r.get(9)?,
                snippet: r.get(10)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

// ── Query parser ────────────────────────────────────────────────

/// Parsed components of a search query string.
///
/// Separated from `SearchFilters` because they come from operator
/// syntax in the query box, while filters are the chip toggles.
/// Kept pub for tests only.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ParsedQuery {
    /// Free-text terms and column-scoped terms to feed FTS5.
    /// Each entry is already a valid FTS5 atom (e.g. `alice`,
    /// `subject:"project x"`, `"exact phrase"`).
    fts_atoms: Vec<String>,
    /// `Some(true)` = require unread, `Some(false)` = require read,
    /// `None` = don't filter on read state.
    is_unread: Option<bool>,
    is_flagged: Option<bool>,
    has_attachment: Option<bool>,
    /// Inclusive epoch bounds from `after:` / `before:` / `on:`.
    /// Repeated date operators intersect (later bounds only ever
    /// tighten), matching the parser's overall AND semantics.
    date_from: Option<i64>,
    date_to: Option<i64>,
    /// Folder named via `in:` / `folder:` — overrides the UI scope's
    /// folder because typing an operator is the more explicit intent.
    folder: Option<String>,
    /// `in:anywhere` — drop the folder restriction entirely, even a
    /// scope-supplied one. Wins over `folder` above.
    folder_any: bool,
}

impl ParsedQuery {
    /// Tighten the lower date bound (keep the later of the two).
    fn tighten_from(&mut self, v: i64) {
        self.date_from = Some(self.date_from.map_or(v, |cur| cur.max(v)));
    }

    /// Tighten the upper date bound (keep the earlier of the two).
    fn tighten_to(&mut self, v: i64) {
        self.date_to = Some(self.date_to.map_or(v, |cur| cur.min(v)));
    }
}

impl ParsedQuery {
    fn has_text(&self) -> bool {
        !self.fts_atoms.is_empty()
    }

    /// Build the `MATCH` expression FTS5 consumes. Atoms are joined
    /// with implicit AND (FTS5's default when atoms are whitespace-
    /// separated at the top level).
    fn match_expression(&self) -> String {
        self.fts_atoms.join(" ")
    }
}

/// Parse an operator-prefixed search string into FTS5 atoms + filters.
///
/// Runs in O(n) over the input, no regex. Unknown `op:value` tokens
/// are treated as free text — we never reject user input, we just
/// ignore operators we don't know (forward-compat with future syntax).
pub(crate) fn parse_query(input: &str) -> ParsedQuery {
    let mut out = ParsedQuery::default();
    let mut chars = input.char_indices().peekable();

    while let Some((_, c)) = chars.peek().copied() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        // Read a "token" — either a quoted phrase or a whitespace-
        // delimited word, possibly with an operator prefix.
        if c == '"' {
            let phrase = read_quoted(&mut chars);
            if !phrase.is_empty() {
                out.fts_atoms.push(fts_phrase(&phrase));
            }
            continue;
        }
        let word = read_word(&mut chars);
        if let Some((op, rest)) = split_operator(&word) {
            apply_operator(&mut out, op, rest, &mut chars);
        } else if !word.is_empty() {
            out.fts_atoms.push(fts_term(&word));
        }
    }

    out
}

/// Read characters up to the next unescaped whitespace.
fn read_word(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> String {
    let mut s = String::new();
    while let Some(&(_, c)) = chars.peek() {
        if c.is_whitespace() {
            break;
        }
        s.push(c);
        chars.next();
    }
    s
}

/// Read characters between a starting `"` and the closing `"` (or EOF).
fn read_quoted(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> String {
    chars.next(); // consume opening "
    let mut s = String::new();
    for (_, c) in chars.by_ref() {
        if c == '"' {
            break;
        }
        s.push(c);
    }
    s
}

/// Split `op:value` into `("op", "value")`. Returns `None` if the
/// word doesn't contain a colon, or if it starts with one (URLs
/// like `https://…` would otherwise be treated as `https:` operator).
fn split_operator(word: &str) -> Option<(&str, &str)> {
    let idx = word.find(':')?;
    if idx == 0 || idx + 1 >= word.len() {
        return None;
    }
    let op = &word[..idx];
    let rest = &word[idx + 1..];
    // Only treat as operator if the op part is pure alpha — avoids
    // catching URLs and future `scheme://` patterns.
    if op.chars().all(|c| c.is_ascii_alphabetic()) {
        Some((op, rest))
    } else {
        None
    }
}

fn apply_operator(
    out: &mut ParsedQuery,
    op: &str,
    rest: &str,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) {
    // Operand may be quoted — if `rest` starts with `"`, pull in
    // everything until the closing quote from the remaining stream.
    let operand = if let Some(stripped) = rest.strip_prefix('"') {
        let mut s = String::from(stripped);
        let mut closed = stripped.ends_with('"') && !stripped.is_empty();
        if closed {
            s.pop();
        }
        while !closed {
            match chars.next() {
                Some((_, '"')) => closed = true,
                Some((_, c)) => s.push(c),
                None => break,
            }
        }
        s
    } else {
        rest.to_string()
    };

    match op.to_ascii_lowercase().as_str() {
        "from" => out.fts_atoms.push(fts_column("from_addr", &operand)),
        "to" => out.fts_atoms.push(fts_column("to_addrs", &operand)),
        "cc" => out.fts_atoms.push(fts_column("cc_addrs", &operand)),
        "subject" | "title" => out.fts_atoms.push(fts_column("subject", &operand)),
        "body" => out.fts_atoms.push(fts_column("body", &operand)),
        "has" | "hasattachment" | "hasattachments" => {
            if matches!(
                operand.to_ascii_lowercase().as_str(),
                "attachment" | "attachments" | "yes" | "true" | "1"
            ) {
                out.has_attachment = Some(true);
            }
        }
        "is" | "state" => match operand.to_ascii_lowercase().as_str() {
            "unread" | "new" => out.is_unread = Some(true),
            "read" | "seen" => out.is_unread = Some(false),
            "flagged" | "starred" | "important" => out.is_flagged = Some(true),
            _ => {}
        },
        // Date operators. `after:` is on-or-after (the named day counts),
        // `before:` is strictly-before — together `after:X before:Y` is
        // the half-open range [X, Y). An unparseable operand falls back
        // to free text below, same as unknown operators.
        "after" | "since" => match parse_date_period(&operand) {
            Some(p) => out.tighten_from(p.start_epoch()),
            None => push_fallback(out, op, &operand),
        },
        "before" => match parse_date_period(&operand) {
            Some(p) => out.tighten_to(p.start_epoch() - 1),
            None => push_fallback(out, op, &operand),
        },
        "on" => match parse_date_period(&operand) {
            Some(p) => {
                out.tighten_from(p.start_epoch());
                out.tighten_to(p.end_epoch());
            }
            None => push_fallback(out, op, &operand),
        },
        // Folder scoping. `in:anywhere` (or `all`/`any`) widens back
        // out to every folder; anything else names a folder to search.
        "in" | "folder" => {
            if matches!(
                operand.to_ascii_lowercase().as_str(),
                "anywhere" | "all" | "any"
            ) {
                out.folder_any = true;
            } else if !operand.is_empty() {
                out.folder = Some(operand);
            }
        }
        // Unknown operator — fall back to a free-text atom so the
        // user's typing still does something useful.
        _ => push_fallback(out, op, &operand),
    }
}

/// Rebuild an `op:value` token as a free-text atom. Used both for
/// unknown operators and for known operators with operands we can't
/// interpret (e.g. a half-typed date) — we never reject user input.
fn push_fallback(out: &mut ParsedQuery, op: &str, operand: &str) {
    let rebuilt = format!("{op}:{operand}");
    out.fts_atoms.push(fts_term(&rebuilt));
}

// ── Date operands ───────────────────────────────────────────────

/// The calendar period a date operand names — a day, a whole month,
/// or a whole year. Public so the IMAP search-criterion builder in
/// the app crate can reuse the exact same operand grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatePeriod {
    /// First day of the period.
    pub start: NaiveDate,
    /// First day *after* the period (exclusive end).
    pub next_start: NaiveDate,
}

impl DatePeriod {
    /// Inclusive epoch-seconds start: local midnight of `start`.
    pub fn start_epoch(&self) -> i64 {
        local_midnight_epoch(self.start)
    }

    /// Inclusive epoch-seconds end: one second before local midnight
    /// of `next_start`.
    pub fn end_epoch(&self) -> i64 {
        local_midnight_epoch(self.next_start) - 1
    }
}

/// Epoch seconds of local midnight on `date`. On DST transitions
/// where midnight doesn't exist or exists twice, take the earliest
/// valid interpretation — being off by an hour twice a year beats
/// dropping the operator on the floor.
fn local_midnight_epoch(date: NaiveDate) -> i64 {
    let midnight = date.and_hms_opt(0, 0, 0).expect("00:00:00 is valid");
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| Utc.from_utc_datetime(&midnight).timestamp())
}

/// Parse a date operand into the period it names.
///
/// Accepted forms:
/// - `YYYY-MM-DD` / `YYYY/MM/DD` — one day
/// - `DD.MM.YYYY`                — one day (common European order)
/// - `YYYY-MM` / `YYYY/MM`       — the whole month
/// - `YYYY`                      — the whole year
///
/// Returns `None` for anything else (including out-of-range dates)
/// so callers can fall back to treating the token as free text.
pub fn parse_date_period(s: &str) -> Option<DatePeriod> {
    let s = s.trim();
    if s.contains('.') {
        // DD.MM.YYYY
        let mut it = s.split('.');
        let (d, m, y) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        let date = NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)?;
        return Some(DatePeriod {
            start: date,
            next_start: date.succ_opt()?,
        });
    }

    let parts: Vec<&str> = s.split(['-', '/']).collect();
    match parts.as_slice() {
        [y] => {
            let year: i32 = y.parse().ok()?;
            // Require a plausible 4-digit year — a bare `in:2026` should
            // parse, but `before:5` is more likely a typo than year 5.
            if !(1000..=9999).contains(&year) {
                return None;
            }
            Some(DatePeriod {
                start: NaiveDate::from_ymd_opt(year, 1, 1)?,
                next_start: NaiveDate::from_ymd_opt(year + 1, 1, 1)?,
            })
        }
        [y, m] => {
            let (year, month): (i32, u32) = (y.parse().ok()?, m.parse().ok()?);
            let start = NaiveDate::from_ymd_opt(year, month, 1)?;
            let next_start = if month == 12 {
                NaiveDate::from_ymd_opt(year + 1, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(year, month + 1, 1)?
            };
            Some(DatePeriod { start, next_start })
        }
        [y, m, d] => {
            let date = NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)?;
            Some(DatePeriod {
                start: date,
                next_start: date.succ_opt()?,
            })
        }
        _ => None,
    }
}

/// Sanitise and wrap a free-text term for FTS5.
///
/// We want "as-you-type" matching — a user typing `dam` should already
/// see hits for `damm`, `damage`, etc. FTS5 expresses that with the
/// `term*` prefix-match operator, but that operator is only valid on
/// bare (unquoted) tokens.
///
/// So: if the word is made of plain letters/digits we emit `word*`.
/// If it contains anything else (punctuation, operators, URL bits),
/// we fall back to a quoted phrase — no prefix, but FTS5 will still
/// tokenize inside the quotes and find it.
fn fts_term(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    let all_safe = word.chars().all(|c| c.is_alphanumeric() || c == '_');
    if all_safe {
        format!("{word}*")
    } else {
        let escaped = word.replace('"', "\"\"");
        format!("\"{escaped}\"")
    }
}

/// Wrap a phrase in FTS5 phrase syntax. Unlike [`fts_term`], phrases
/// are always quoted — the user explicitly asked for exact match by
/// typing the surrounding quotes, so we don't want to silently convert
/// them into a prefix search.
fn fts_phrase(phrase: &str) -> String {
    let escaped = phrase.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Column-scoped atom: `column : "value"`.
fn fts_column(column: &str, value: &str) -> String {
    format!("{column}:{}", fts_term(value))
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use chrono::Duration;
    use unkai_core::models::Email;

    fn open() -> Cache {
        Cache::open_in_memory().expect("open in-memory cache")
    }

    fn email(uid: u32, folder: &str, subject: &str, body: &str, from: &str) -> Email {
        Email {
            id: format!("{folder}:{uid}"),
            account_id: "acc".into(),
            folder: folder.into(),
            from: from.into(),
            to: vec!["bob@example.com".into()],
            cc: vec![],
            subject: subject.into(),
            body_text: Some(body.into()),
            body_html: None,
            date: Utc::now() - Duration::seconds(uid as i64),
            is_read: false,
            is_starred: false,
            has_attachments: false,
            attachments: vec![],
            message_id: None,
            in_reply_to: None,
            references_ids: Vec::new(),
            protection: None,
            signature_status: None,
            signer_fingerprint: None,
            is_pinned: false,
            reminder_at: None,
            priority: None,
            priority_override: None,
            mdn_requested_to: None,
            mdn_handled: None,
        }
    }

    #[test]
    fn parse_free_text() {
        // Free-text words are emitted as FTS5 prefix atoms so typing
        // `dam` already matches `damm`, `damage`, etc.
        let p = parse_query("project budget");
        assert_eq!(p.fts_atoms, vec!["project*", "budget*"]);
        assert!(p.has_text());
    }

    #[test]
    fn parse_operators() {
        let p = parse_query("from:alice subject:\"weekly update\" has:attachment");
        // `alice` is a bare word → prefix-match. `weekly update` is a
        // quoted phrase → stays an exact phrase.
        assert_eq!(
            p.fts_atoms,
            vec!["from_addr:alice*", "subject:\"weekly update\"",]
        );
        assert_eq!(p.has_attachment, Some(true));
    }

    #[test]
    fn parse_state_operators() {
        let p = parse_query("is:unread is:flagged");
        assert!(p.fts_atoms.is_empty());
        assert_eq!(p.is_unread, Some(true));
        assert_eq!(p.is_flagged, Some(true));
    }

    #[test]
    fn parse_ignores_urls() {
        // A `scheme://host` in the query should not be parsed as
        // `scheme:` operator (non-alpha after colon wins, plus the
        // slashes stay in the term).
        let p = parse_query("https://example.com/foo");
        assert_eq!(p.fts_atoms.len(), 1);
        assert!(p.fts_atoms[0].contains("https"));
    }

    #[test]
    fn parse_quoted_phrase() {
        let p = parse_query("\"quarterly report\" urgent");
        // Quoted phrase stays exact; the bare word becomes prefix-match.
        assert_eq!(p.fts_atoms[0], "\"quarterly report\"");
        assert_eq!(p.fts_atoms[1], "urgent*");
    }

    /// Local noon on the given day, as the UTC instant the cache
    /// stores. Noon keeps the tests timezone-proof: whatever offset
    /// the machine runs, noon local is safely inside the local day
    /// the date operators resolve against.
    fn local_noon(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        let naive = NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn parse_date_periods() {
        let day = parse_date_period("2026-01-31").unwrap();
        assert_eq!(day.start, NaiveDate::from_ymd_opt(2026, 1, 31).unwrap());
        assert_eq!(day.next_start, NaiveDate::from_ymd_opt(2026, 2, 1).unwrap());

        // Alternative separators and the European day-first order all
        // name the same day.
        assert_eq!(parse_date_period("2026/01/31"), Some(day));
        assert_eq!(parse_date_period("31.01.2026"), Some(day));

        let month = parse_date_period("2026-03").unwrap();
        assert_eq!(month.start, NaiveDate::from_ymd_opt(2026, 3, 1).unwrap());
        assert_eq!(
            month.next_start,
            NaiveDate::from_ymd_opt(2026, 4, 1).unwrap()
        );

        let year = parse_date_period("2026").unwrap();
        assert_eq!(year.start, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        assert_eq!(
            year.next_start,
            NaiveDate::from_ymd_opt(2027, 1, 1).unwrap()
        );

        // Nonsense stays unparsed so the caller can treat it as text.
        assert_eq!(parse_date_period("tomorrow"), None);
        assert_eq!(parse_date_period("2026-13-01"), None);
        assert_eq!(parse_date_period("5"), None);
    }

    #[test]
    fn parse_date_operators() {
        let p = parse_query("after:2026-01-01 before:2026-02-01");
        assert!(p.fts_atoms.is_empty());
        let jan = parse_date_period("2026-01-01").unwrap();
        let feb = parse_date_period("2026-02-01").unwrap();
        assert_eq!(p.date_from, Some(jan.start_epoch()));
        // `before:` is strictly-before → upper bound ends the moment
        // the named day starts.
        assert_eq!(p.date_to, Some(feb.start_epoch() - 1));
    }

    #[test]
    fn parse_on_month_covers_whole_month() {
        let p = parse_query("on:2026-03");
        let march = parse_date_period("2026-03").unwrap();
        assert_eq!(p.date_from, Some(march.start_epoch()));
        assert_eq!(p.date_to, Some(march.end_epoch()));
    }

    #[test]
    fn parse_repeated_date_operators_intersect() {
        // Two lower bounds → the later one wins (AND semantics).
        let p = parse_query("after:2026-01-01 after:2026-06-01");
        let june = parse_date_period("2026-06-01").unwrap();
        assert_eq!(p.date_from, Some(june.start_epoch()));
    }

    #[test]
    fn parse_invalid_date_falls_back_to_text() {
        let p = parse_query("before:lunch");
        assert_eq!(p.date_to, None);
        assert_eq!(p.fts_atoms, vec!["\"before:lunch\""]);
    }

    #[test]
    fn parse_folder_operators() {
        let p = parse_query("in:Sent");
        assert_eq!(p.folder.as_deref(), Some("Sent"));
        assert!(!p.folder_any);

        let p = parse_query("folder:\"Project X\" budget");
        assert_eq!(p.folder.as_deref(), Some("Project X"));
        assert_eq!(p.fts_atoms, vec!["budget*"]);

        let p = parse_query("in:anywhere");
        assert!(p.folder_any);
        assert_eq!(p.folder, None);
    }

    #[test]
    fn search_matches_subject_and_body() {
        let cache = open();
        cache
            .upsert_message(&email(
                1,
                "INBOX",
                "Budget planning Q2",
                "Please review the attached spreadsheet.",
                "alice@example.com",
            ))
            .unwrap();
        cache
            .upsert_message(&email(
                2,
                "INBOX",
                "Lunch tomorrow?",
                "Want to grab lunch?",
                "bob@example.com",
            ))
            .unwrap();

        let hits = cache
            .search_emails("budget", &SearchScope::default(), &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject, "Budget planning Q2");
        assert!(hits[0].snippet.contains("<mark>"));
    }

    #[test]
    fn search_hit_carries_protection() {
        // #440: consumers that hand snippets to third parties (the
        // MCP mail tools) decide redaction from the hit itself, so
        // the body row's protection has to ride along.
        let cache = open();
        let mut encrypted = email(1, "INBOX", "Plans", "the secret rendezvous point", "a@x.de");
        encrypted.protection = Some("encrypted".into());
        cache.upsert_message(&encrypted).unwrap();
        cache
            .upsert_message(&email(2, "INBOX", "Plans", "the public agenda", "b@x.de"))
            .unwrap();

        let hits = cache
            .search_emails("plans", &SearchScope::default(), &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 2);
        let by_uid = |uid| hits.iter().find(|h| h.uid == uid).unwrap();
        assert_eq!(by_uid(1).protection.as_deref(), Some("encrypted"));
        assert_eq!(by_uid(2).protection, None);

        // Pure-filter queries go through the no-FTS branch — the
        // column must be projected there too.
        let filters = SearchFilters {
            unread_only: true,
            ..Default::default()
        };
        let hits = cache
            .search_emails("", &SearchScope::default(), &filters)
            .unwrap();
        assert_eq!(hits.len(), 2);
        let by_uid = |uid| hits.iter().find(|h| h.uid == uid).unwrap();
        assert_eq!(by_uid(1).protection.as_deref(), Some("encrypted"));
    }

    #[test]
    fn search_filters_by_scope() {
        let cache = open();
        cache
            .upsert_message(&email(1, "INBOX", "Alpha", "hello", "a@x.de"))
            .unwrap();
        cache
            .upsert_message(&email(2, "Sent", "Alpha", "hello", "a@x.de"))
            .unwrap();

        let scope = SearchScope {
            account_id: Some("acc".into()),
            folder: Some("Sent".into()),
            limit: 10,
        };
        let hits = cache
            .search_emails("alpha", &scope, &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].folder, "Sent");
    }

    #[test]
    fn search_operator_from() {
        let cache = open();
        cache
            .upsert_message(&email(1, "INBOX", "Hi", "body", "alice@example.com"))
            .unwrap();
        cache
            .upsert_message(&email(2, "INBOX", "Hi", "body", "bob@example.com"))
            .unwrap();

        let hits = cache
            .search_emails(
                "from:alice",
                &SearchScope::default(),
                &SearchFilters::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].from.contains("alice"));
    }

    #[test]
    fn search_operators_combine_with_and() {
        // Multiple operators (the advanced-search panel emits e.g.
        // `from:… subject:…`) must intersect — a hit has to satisfy
        // every operator, not any one of them.
        let cache = open();
        cache
            .upsert_message(&email(
                1,
                "INBOX",
                "Status report",
                "body",
                "alice@example.com",
            ))
            .unwrap();
        cache
            .upsert_message(&email(
                2,
                "INBOX",
                "Lunch plans",
                "body",
                "alice@example.com",
            ))
            .unwrap();
        cache
            .upsert_message(&email(
                3,
                "INBOX",
                "Status report",
                "body",
                "bob@example.com",
            ))
            .unwrap();

        // Bare-word operands, typed by hand.
        let hits = cache
            .search_emails(
                "from:alice subject:report",
                &SearchScope::default(),
                &SearchFilters::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "bare operands must AND");
        assert_eq!(hits[0].uid, 1);

        // Full-address operand — what a contact pick in the advanced
        // panel commits into `from:`.
        let hits = cache
            .search_emails(
                "from:alice@example.com subject:report",
                &SearchScope::default(),
                &SearchFilters::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "address operand must AND with subject");
        assert_eq!(hits[0].uid, 1);

        // Operator + state filter + free text all intersect too.
        let hits = cache
            .search_emails(
                "report from:bob",
                &SearchScope::default(),
                &SearchFilters::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "free text must AND with operators");
        assert_eq!(hits[0].uid, 3);
    }

    #[test]
    fn search_filter_unread_only() {
        let cache = open();
        let mut read_mail = email(1, "INBOX", "one", "body", "a@x.de");
        read_mail.is_read = true;
        let unread_mail = email(2, "INBOX", "two", "body", "b@x.de");
        cache.upsert_message(&read_mail).unwrap();
        cache.upsert_message(&unread_mail).unwrap();

        let filters = SearchFilters {
            unread_only: true,
            ..Default::default()
        };
        let hits = cache
            .search_emails("", &SearchScope::default(), &filters)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].uid, 2);
    }

    #[test]
    fn search_filter_has_attachment() {
        let cache = open();
        let plain = email(1, "INBOX", "no att", "body", "a@x.de");
        let mut att = email(2, "INBOX", "with att", "body", "b@x.de");
        att.has_attachments = true;
        cache.upsert_message(&plain).unwrap();
        cache.upsert_message(&att).unwrap();

        let hits = cache
            .search_emails(
                "",
                &SearchScope::default(),
                &SearchFilters {
                    has_attachment: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].has_attachments);
    }

    #[test]
    fn search_empty_query_returns_recent() {
        let cache = open();
        for i in 0..5 {
            cache
                .upsert_message(&email(i + 1, "INBOX", &format!("subj {i}"), "b", "a@x.de"))
                .unwrap();
        }
        let hits = cache
            .search_emails("", &SearchScope::default(), &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn search_matches_partial_prefix() {
        // User typed "dam" — we expect hits on "damm" and "damage"
        // via FTS5's prefix-match operator, not just exact "dam".
        let cache = open();
        cache
            .upsert_message(&email(1, "INBOX", "Damm das Tor", "body", "a@x.de"))
            .unwrap();
        cache
            .upsert_message(&email(
                2,
                "INBOX",
                "report",
                "damage assessment overdue",
                "b@x.de",
            ))
            .unwrap();
        cache
            .upsert_message(&email(3, "INBOX", "unrelated", "nothing", "c@x.de"))
            .unwrap();

        let hits = cache
            .search_emails("dam", &SearchScope::default(), &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 2);
        // Snippet should wrap the partial match.
        assert!(hits.iter().all(|h| h.snippet.contains("<mark>")));
    }

    #[test]
    fn search_results_newest_first() {
        let cache = open();
        cache
            .upsert_message(&email(1, "INBOX", "budget old", "b", "a@x.de"))
            .unwrap();
        cache
            .upsert_message(&email(2, "INBOX", "budget new", "b", "a@x.de"))
            .unwrap();

        let hits = cache
            .search_emails("budget", &SearchScope::default(), &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 2);
        // uid 1 has the smaller offset per the `email()` helper
        // (`now - uid seconds`), so it's the newer message.
        assert_eq!(hits[0].uid, 1);
        assert_eq!(hits[1].uid, 2);
    }

    /// Three messages spread across three months, searched with every
    /// date-operator shape.
    #[test]
    fn search_date_operators() {
        let cache = open();
        let dates = [
            (1, local_noon(2026, 1, 15)),
            (2, local_noon(2026, 2, 15)),
            (3, local_noon(2026, 3, 15)),
        ];
        for (uid, date) in dates {
            let mut e = email(uid, "INBOX", "monthly report", "body", "a@x.de");
            e.date = date;
            cache.upsert_message(&e).unwrap();
        }

        let uids = |query: &str| -> Vec<u32> {
            let mut hits: Vec<u32> = cache
                .search_emails(query, &SearchScope::default(), &SearchFilters::default())
                .unwrap()
                .into_iter()
                .map(|h| h.uid)
                .collect();
            hits.sort_unstable();
            hits
        };

        assert_eq!(uids("report after:2026-02-01"), vec![2, 3]);
        assert_eq!(uids("report before:2026-02-01"), vec![1]);
        assert_eq!(uids("report on:2026-02-15"), vec![2]);
        assert_eq!(uids("report on:2026-02"), vec![2]);
        assert_eq!(uids("report after:2026-01-20 before:2026-03-01"), vec![2]);
        // Date operators also work with no text at all (pure-filter
        // branch, no FTS join).
        assert_eq!(uids("on:2026-03"), vec![3]);
    }

    #[test]
    fn search_in_operator_overrides_scope() {
        let cache = open();
        cache
            .upsert_message(&email(1, "INBOX", "Alpha", "hello", "a@x.de"))
            .unwrap();
        cache
            .upsert_message(&email(2, "Sent", "Alpha", "hello", "a@x.de"))
            .unwrap();

        let inbox_scope = SearchScope {
            account_id: Some("acc".into()),
            folder: Some("INBOX".into()),
            limit: 10,
        };

        // `in:` beats the scope's folder — lower-case operand matches
        // the real folder name case-insensitively.
        let hits = cache
            .search_emails("alpha in:sent", &inbox_scope, &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].folder, "Sent");

        // `in:anywhere` widens a folder-scoped search to all folders.
        let hits = cache
            .search_emails("alpha in:anywhere", &inbox_scope, &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 2);

        // Without either operator the scope still applies.
        let hits = cache
            .search_emails("alpha", &inbox_scope, &SearchFilters::default())
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].folder, "INBOX");
    }
}

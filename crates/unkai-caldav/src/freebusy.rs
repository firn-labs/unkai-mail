//! CalDAV free-busy queries (RFC 4791 §7.10) — "is this principal
//! booked at time T?" without surfacing event details.
//!
//! Used by the EventPlanner UI to render each attendee's busy
//! density across a time window before the user picks a slot.
//!
//! # Two request shapes
//!
//! - **`free-busy-query` REPORT** against a calendar collection or
//!   home — returns an iCalendar body with a single VFREEBUSY
//!   component listing busy periods. Fast, privacy-friendly (the
//!   server only emits busy/free, never event titles), but requires
//!   the caller to have at least read-access to the target
//!   calendar(s).
//!
//! - The CalDAV scheduling-outbox POST (RFC 6638 §6.2) is the
//!   alternative: any user can ask any other user's outbox for
//!   their availability without explicit calendar-sharing. We
//!   don't implement that path yet — most NC deployments share
//!   internal calendars by default and the REPORT version is
//!   simpler. If sharing is locked down, the planner returns
//!   "no signal" for that user and the UI falls back to local-
//!   cache scanning (events where they're listed as attendee).
//!
//! # Output shape
//!
//! [`BusyPeriod`] is the minimal shape the UI needs: a UTC
//! start/end pair plus a coarse type (busy / tentative / free).
//! We don't carry the source event's UID or summary — the
//! whole point of free-busy is that the queryer only sees
//! "blocked", not "blocked because of $event".

use chrono::{DateTime, Duration, TimeZone, Utc};
use reqwest::StatusCode;

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;

use crate::client::{build, normalize_server_url, report};

/// A busy / free period on someone's calendar, derived from a
/// `FREEBUSY:` line in a VFREEBUSY response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyPeriod {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub kind: BusyKind,
}

/// RFC 5545 `FBTYPE` values, normalised to the three the planner UI
/// distinguishes visually.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyKind {
    /// `FBTYPE=BUSY` (or no FBTYPE — the default).
    Busy,
    /// `FBTYPE=BUSY-TENTATIVE` — meeting requested but not confirmed.
    Tentative,
    /// `FBTYPE=BUSY-UNAVAILABLE` — out-of-office / day off.
    Unavailable,
    /// `FBTYPE=FREE` — explicitly emitted free slot. We rarely see
    /// these but parse them so the planner can render "explicitly
    /// available" if a server bothers to send it.
    Free,
}

const FREE_BUSY_BODY_TEMPLATE: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<C:free-busy-query xmlns:C="urn:ietf:params:xml:ns:caldav">
  <C:time-range start="{START}" end="{END}"/>
</C:free-busy-query>"#;

/// REPORT a `free-busy-query` against a target calendar home and
/// return the busy periods within `[start, end)`.
///
/// `target_principal_url` is typically `<server>/remote.php/dav/
/// calendars/<user_id>/` — Sabre/DAV (NC's CalDAV stack) accepts
/// the REPORT against either the home collection or any of its
/// child calendars; the home is what we want because it
/// aggregates across all the target's calendars in one shot.
///
/// `username` / `app_password` are the *requesting* user's
/// credentials. The server checks read-access to the target's
/// calendars before answering, so a user without access gets
/// 403 / 404 → returned as `Err(UnkaiError::Nextcloud)` for
/// the caller to soft-fail on.
pub async fn query_free_busy(
    target_principal_url: &str,
    username: &str,
    app_password: &str,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<BusyPeriod>, UnkaiError> {
    let http = build(trusted_certs)?;
    let body = FREE_BUSY_BODY_TEMPLATE
        .replace("{START}", &fmt_utc_basic(range_start))
        .replace("{END}", &fmt_utc_basic(range_end));

    let resp = report(&http, target_principal_url, username, app_password, &body).await?;
    let status = resp.status();
    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Err(UnkaiError::Nextcloud(format!(
            "free-busy-query refused for {target_principal_url} (HTTP {status})"
        )));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "free-busy-query returned HTTP {status}"
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading free-busy body: {e}")))?;
    Ok(parse_vfreebusy(&text))
}

/// Build the `/remote.php/dav/calendars/{user_id}/` URL the
/// REPORT is sent against. Convenience for the common Nextcloud
/// case where the principal home is well-known.
pub fn nc_principal_home(server_url: &str, user_id: &str) -> String {
    format!(
        "{}/remote.php/dav/calendars/{user_id}/",
        normalize_server_url(server_url)
    )
}

/// Parse a VFREEBUSY iCalendar body into busy periods. Tolerant
/// of:
///   - Lines folded per RFC 5545 §3.1 (column 75 + leading space).
///   - Multiple FREEBUSY periods comma-separated on a single line.
///   - Periods expressed as `start/end` or `start/duration`.
///
/// Anything we can't parse is skipped with a warning rather than
/// failing the whole response — a single malformed FREEBUSY
/// shouldn't blank out the planner.
pub fn parse_vfreebusy(body: &str) -> Vec<BusyPeriod> {
    let unfolded = unfold_lines(body);
    let mut out = Vec::new();
    let mut in_freebusy = false;
    for line in unfolded.lines() {
        let line = line.trim_end_matches('\r');
        if line.eq_ignore_ascii_case("BEGIN:VFREEBUSY") {
            in_freebusy = true;
            continue;
        }
        if line.eq_ignore_ascii_case("END:VFREEBUSY") {
            in_freebusy = false;
            continue;
        }
        if !in_freebusy {
            continue;
        }
        if !line.to_ascii_uppercase().starts_with("FREEBUSY") {
            continue;
        }
        // Split into property header + value: `FREEBUSY[;params]:value`.
        let Some((header, value)) = line.split_once(':') else {
            continue;
        };
        let kind = parse_fbtype(header);
        // Each value may carry comma-separated periods.
        for period in value.split(',') {
            if let Some(p) = parse_period(period.trim(), kind) {
                out.push(p);
            } else {
                tracing::warn!("free-busy: skipping unparseable period {:?}", period);
            }
        }
    }
    out
}

fn parse_fbtype(header: &str) -> BusyKind {
    // Header may look like `FREEBUSY` or `FREEBUSY;FBTYPE=BUSY-TENTATIVE`.
    for part in header.split(';').skip(1) {
        let upper = part.to_ascii_uppercase();
        if let Some(rest) = upper.strip_prefix("FBTYPE=") {
            return match rest {
                "FREE" => BusyKind::Free,
                "BUSY-TENTATIVE" => BusyKind::Tentative,
                "BUSY-UNAVAILABLE" => BusyKind::Unavailable,
                // "BUSY" or anything we don't recognise → busy.
                _ => BusyKind::Busy,
            };
        }
    }
    BusyKind::Busy
}

fn parse_period(s: &str, kind: BusyKind) -> Option<BusyPeriod> {
    let (lhs, rhs) = s.split_once('/')?;
    let start = parse_utc_basic(lhs)?;
    let end = if rhs.starts_with('P') || rhs.starts_with('+') || rhs.starts_with('-') {
        let dur = parse_iso_duration(rhs)?;
        start.checked_add_signed(dur)?
    } else {
        parse_utc_basic(rhs)?
    };
    if end <= start {
        return None;
    }
    Some(BusyPeriod { start, end, kind })
}

/// Parse the basic-format UTC timestamp CalDAV uses
/// (`YYYYMMDDTHHMMSSZ`).
fn parse_utc_basic(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.len() != 16 || !s.ends_with('Z') {
        return None;
    }
    let ndt = chrono::NaiveDateTime::parse_from_str(&s[..15], "%Y%m%dT%H%M%S").ok()?;
    Some(Utc.from_utc_datetime(&ndt))
}

/// Tiny ISO-8601 duration parser covering the shapes we see in
/// free-busy responses: `PT15M`, `PT2H`, `PT1H30M`, `P1D`.
/// Negative durations are not valid for FREEBUSY periods so we
/// reject them.
fn parse_iso_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if !s.starts_with('P') {
        return None;
    }
    let mut chars = s[1..].chars().peekable();
    let mut days = 0i64;
    let mut hours = 0i64;
    let mut minutes = 0i64;
    let mut seconds = 0i64;
    let mut in_time = false;
    let mut buf = String::new();
    while let Some(c) = chars.next() {
        if c == 'T' {
            in_time = true;
            continue;
        }
        if c.is_ascii_digit() {
            buf.push(c);
            continue;
        }
        let n: i64 = buf.parse().ok()?;
        buf.clear();
        match (in_time, c) {
            (false, 'D') => days = n,
            (true, 'H') => hours = n,
            (true, 'M') => minutes = n,
            (true, 'S') => seconds = n,
            // 'W' (weeks) is also valid but we never see it for
            // free-busy; ignore so we don't return an outright
            // error.
            _ => return None,
        }
    }
    let total = days * 86_400 + hours * 3600 + minutes * 60 + seconds;
    if total <= 0 {
        return None;
    }
    Some(Duration::seconds(total))
}

/// `2026-05-09T17:00:00Z` → `20260509T170000Z`.
fn fmt_utc_basic(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// RFC 5545 §3.1 — a continuation line starts with a single
/// space or tab. Drop the prefix and concatenate to the previous.
fn unfold_lines(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for (i, line) in body.split('\n').enumerate() {
        let trimmed = line.trim_end_matches('\r');
        if i > 0 && (trimmed.starts_with(' ') || trimmed.starts_with('\t')) {
            // Drop the single leading space/tab; concatenate.
            out.push_str(&trimmed[1..]);
        } else {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(trimmed);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn parses_basic_busy_period() {
        let body = "BEGIN:VCALENDAR\r\n\
                    VERSION:2.0\r\n\
                    BEGIN:VFREEBUSY\r\n\
                    DTSTART:20260509T000000Z\r\n\
                    DTEND:20260509T235959Z\r\n\
                    FREEBUSY:20260509T140000Z/20260509T150000Z\r\n\
                    END:VFREEBUSY\r\n\
                    END:VCALENDAR\r\n";
        let periods = parse_vfreebusy(body);
        assert_eq!(periods.len(), 1);
        assert_eq!(periods[0].start, ts(2026, 5, 9, 14, 0));
        assert_eq!(periods[0].end, ts(2026, 5, 9, 15, 0));
        assert_eq!(periods[0].kind, BusyKind::Busy);
    }

    #[test]
    fn parses_fbtype_variants_and_durations() {
        let body = "BEGIN:VFREEBUSY\r\n\
                    FREEBUSY;FBTYPE=BUSY-TENTATIVE:20260509T100000Z/PT30M\r\n\
                    FREEBUSY;FBTYPE=BUSY-UNAVAILABLE:20260509T120000Z/PT1H\r\n\
                    FREEBUSY;FBTYPE=FREE:20260509T130000Z/20260509T140000Z\r\n\
                    END:VFREEBUSY\r\n";
        let periods = parse_vfreebusy(body);
        assert_eq!(periods.len(), 3);
        assert_eq!(periods[0].kind, BusyKind::Tentative);
        assert_eq!(periods[0].end, ts(2026, 5, 9, 10, 30));
        assert_eq!(periods[1].kind, BusyKind::Unavailable);
        assert_eq!(periods[1].end, ts(2026, 5, 9, 13, 0));
        assert_eq!(periods[2].kind, BusyKind::Free);
    }

    #[test]
    fn parses_comma_separated_periods_on_one_line() {
        let body = "BEGIN:VFREEBUSY\r\n\
                    FREEBUSY:20260509T100000Z/PT30M,20260509T110000Z/PT30M\r\n\
                    END:VFREEBUSY\r\n";
        let periods = parse_vfreebusy(body);
        assert_eq!(periods.len(), 2);
        assert_eq!(periods[0].start, ts(2026, 5, 9, 10, 0));
        assert_eq!(periods[1].start, ts(2026, 5, 9, 11, 0));
    }

    #[test]
    fn ignores_lines_outside_vfreebusy() {
        // A FREEBUSY-shaped line outside any VFREEBUSY block must
        // not leak into the result.
        let body = "FREEBUSY:20260509T100000Z/PT30M\r\n\
                    BEGIN:VFREEBUSY\r\n\
                    FREEBUSY:20260509T200000Z/PT30M\r\n\
                    END:VFREEBUSY\r\n";
        let periods = parse_vfreebusy(body);
        assert_eq!(periods.len(), 1);
        assert_eq!(periods[0].start, ts(2026, 5, 9, 20, 0));
    }

    #[test]
    fn unfolds_continuation_lines() {
        // RFC 5545: a 75-char fold splits the line; the
        // continuation is prefixed with a single space.
        let body = "BEGIN:VFREEBUSY\r\nFREEBUSY:202605\r\n 09T100000Z/PT30M\r\nEND:VFREEBUSY\r\n";
        let periods = parse_vfreebusy(body);
        assert_eq!(periods.len(), 1);
        assert_eq!(periods[0].start, ts(2026, 5, 9, 10, 0));
    }

    #[test]
    fn duration_parser_handles_common_shapes() {
        assert_eq!(
            parse_iso_duration("PT15M"),
            Some(Duration::seconds(15 * 60))
        );
        assert_eq!(parse_iso_duration("PT2H"), Some(Duration::seconds(7200)));
        assert_eq!(parse_iso_duration("PT1H30M"), Some(Duration::seconds(5400)));
        assert_eq!(parse_iso_duration("P1D"), Some(Duration::seconds(86_400)));
        assert!(parse_iso_duration("P0D").is_none());
    }

    #[test]
    fn nc_principal_home_uses_well_known_path() {
        assert_eq!(
            nc_principal_home("https://nc.example.com/", "alice"),
            "https://nc.example.com/remote.php/dav/calendars/alice/"
        );
    }
}

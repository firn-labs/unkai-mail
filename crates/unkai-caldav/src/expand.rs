//! RFC 5545 recurrence expansion.
//!
//! Issue #47 slice 4. The sync/store pipeline caches the VEVENT master
//! plus any `RECURRENCE-ID` overrides as separate rows. To actually
//! *render* a calendar, we need concrete occurrences over the visible
//! window: expand the master's `RRULE`, splice in `RDATE` extras, drop
//! `EXDATE` cancellations, and swap in overrides for any instance that
//! was modified server-side.
//!
//! # Why the `rrule` crate
//!
//! Recurrence looks simple until it isn't — UNTIL with TZID, BYDAY
//! combined with COUNT, DST shifts that slide a 09:00 meeting forward
//! or backward an hour, yearly BYMONTHDAY=-1 for "last day of the
//! month"… the RFC has a lot of corners. `rrule` is a pure-Rust
//! implementation that passes the upstream RFC test corpus, so we let
//! it do the heavy lifting and keep this module to orchestration.
//!
//! # Override + EXDATE semantics
//!
//! - An override's `recurrence_id` names the *original* occurrence it
//!   replaces. When expansion produces an instance whose time matches
//!   an override's RID, we emit the override's data in place of the
//!   synthetic instance. The server already put the override inside
//!   the master's `EXDATE`, which keeps the expander from
//!   double-counting, but we also belt-and-brace the match here in
//!   case a server skips the EXDATE.
//! - An override whose *new* start falls outside the window is
//!   silently dropped — the user asked for "this date range" and the
//!   override has effectively been moved out.
//!
//! # Failure mode
//!
//! A malformed RRULE (rare but possible with home-grown calendar
//! servers) would otherwise blank out an entire calendar view. We
//! log at `warn` and fall back to rendering the master as a single
//! event so the panel remains useful. The bad series is visible; the
//! others aren't affected.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rrule::{RRuleSet, Tz};
use tracing::warn;

use unkai_core::models::CalendarEvent;

/// Expand one recurring master into concrete occurrences inside
/// `[range_start, range_end)`.
///
/// `overrides` must be the RECURRENCE-ID overrides that correspond to
/// this master (same UID + calendar). Pass an empty slice for events
/// that have none. The caller is responsible for grouping — this
/// function doesn't check UID equality.
///
/// Returns a ready-to-render `Vec<CalendarEvent>`. Each synthesized
/// occurrence gets a stable composite id (`{master.id}::occ::{epoch}`)
/// so Svelte keyed-each loops can tell them apart.
pub fn expand_event(
    master: &CalendarEvent,
    overrides: &[&CalendarEvent],
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> Vec<CalendarEvent> {
    // Fast path: non-recurring event. Only emit when it overlaps the
    // window (same half-open semantics as the SQL range filter).
    if master.rrule.is_none() && master.rdate.is_empty() {
        if master.end >= range_start && master.start < range_end {
            return vec![master.clone()];
        }
        return vec![];
    }

    // Index overrides by the *original* recurrence time they replace.
    // We use epoch seconds as the key because that's the granularity
    // the store round-trips through SQLite — comparing `DateTime<Utc>`
    // values directly would be sensitive to nanosecond drift we don't
    // actually preserve.
    let overrides_by_rid: HashMap<i64, &CalendarEvent> = overrides
        .iter()
        .filter_map(|o| o.recurrence_id.map(|rid| (rid.timestamp(), *o)))
        .collect();

    // Hand the raw RRULE string to the `rrule` parser inside a minimal
    // iCalendar wrapper. Building an `RRuleSet` programmatically is
    // possible but the string route trivially handles the flag soup
    // in `FREQ=MONTHLY;BYSETPOS=-1;BYDAY=FR` etc. without us having to
    // re-tokenise it.
    let text = build_ical_text(master);
    let set: RRuleSet = match text.parse() {
        Ok(s) => s,
        Err(err) => {
            warn!(
                event_id = %master.id,
                error = %err,
                "RRULE parse failed; falling back to master only"
            );
            if master.end >= range_start && master.start < range_end {
                return vec![master.clone()];
            }
            return vec![];
        }
    };

    let tz_start = range_start.with_timezone(&Tz::UTC);
    let tz_end = range_end.with_timezone(&Tz::UTC);
    // `after` / `before` with `inclusive = true` means "include the
    // boundary exactly" — we then tighten to half-open below by
    // filtering `occ >= range_end` out.
    let occurrences = set.after(tz_start).before(tz_end).all_unchecked();

    let duration = master.end - master.start;
    let mut out = Vec::with_capacity(occurrences.len());
    for occ in occurrences {
        let occ_utc = occ.with_timezone(&Utc);
        if occ_utc >= range_end {
            continue;
        }
        if let Some(ov) = overrides_by_rid.get(&occ_utc.timestamp()).copied() {
            // Only surface the override if its *new* time still falls
            // in the window. A user who moved a meeting from Thursday
            // to the next Monday would push it out of a "this week"
            // query — and that's the correct answer.
            if ov.end >= range_start && ov.start < range_end {
                out.push(ov.clone());
            }
            continue;
        }
        // Synthesise an occurrence from the master, shifted to the
        // computed time. The recurrence fields are cleared so the UI
        // never sees an occurrence pretending to be a series itself.
        let mut inst = master.clone();
        inst.id = format!("{}::occ::{}", master.id, occ_utc.timestamp());
        inst.start = occ_utc;
        inst.end = occ_utc + duration;
        inst.rrule = None;
        inst.rdate = Vec::new();
        inst.exdate = Vec::new();
        inst.recurrence_id = Some(occ_utc);
        out.push(inst);
    }
    out
}

/// Build the minimal iCalendar text `RRuleSet::from_str` expects.
///
/// We intentionally emit times as UTC (`Z` suffix). The master's
/// `DateTime<Utc>` already carries the resolved absolute instant —
/// whatever TZID the server originally sent has been converted to UTC
/// during parsing. Expanding in UTC keeps `rrule` on its simplest
/// path; the final `DateTime<Utc>` values are then rendered in the
/// user's local time by the UI layer.
fn build_ical_text(master: &CalendarEvent) -> String {
    let mut s = format!("DTSTART:{}\n", fmt_ical_utc(master.start));
    if let Some(rule) = &master.rrule {
        s.push_str("RRULE:");
        s.push_str(&normalise_rrule(rule));
        s.push('\n');
    }
    if !master.rdate.is_empty() {
        let parts: Vec<String> = master.rdate.iter().map(|d| fmt_ical_utc(*d)).collect();
        s.push_str("RDATE:");
        s.push_str(&parts.join(","));
        s.push('\n');
    }
    if !master.exdate.is_empty() {
        let parts: Vec<String> = master.exdate.iter().map(|d| fmt_ical_utc(*d)).collect();
        s.push_str("EXDATE:");
        s.push_str(&parts.join(","));
        s.push('\n');
    }
    s
}

fn fmt_ical_utc(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Pre-flight fix-up for RRULE strings before we hand them to the
/// `rrule` crate.
///
/// Three real-world quirks motivate this:
///
/// - **UNTIL without `Z`.** Nextcloud's auto-generated birthday
///   calendar (`contact-birthdays`) emits rules like
///   `FREQ=YEARLY;UNTIL=20261231T235959`. We render `DTSTART` in UTC
///   (it's stored that way in the cache), and the strict RFC 5545
///   reading is that `UNTIL` must match `DTSTART`'s form. The crate
///   refuses, which would blank out every contact's birthday. We
///   append `Z` to any bare date-time `UNTIL` so it matches the UTC
///   `DTSTART` we emit.
///
/// - **Date-only UNTIL against UTC DTSTART.** Same birthday calendar,
///   different shape — `FREQ=YEARLY;UNTIL=20261231`. The `rrule`
///   crate reads a bare 8-char date as floating-local, then refuses
///   it because our DTSTART is UTC ("Allowed timezones for UNTIL with
///   the given start date timezone are: ["UTC"]"). We expand the date
///   to end-of-day UTC (`T235959Z`) — semantically the same window
///   end, just in the timezone the parser will accept.
///
/// - **Case mangling.** Some servers echo lowercase parameter names
///   (`until=…`). We normalise the parameter name while we're here so
///   the UNTIL fix-up catches both cases.
fn normalise_rrule(rule: &str) -> String {
    rule.split(';')
        .map(|part| {
            let (name, value) = match part.split_once('=') {
                Some(pair) => pair,
                None => return part.to_string(),
            };
            if !name.eq_ignore_ascii_case("UNTIL") {
                return part.to_string();
            }
            // 15-char `YYYYMMDDTHHMMSS` without a `Z` suffix — add it.
            if value.len() == 15
                && value.as_bytes().get(8) == Some(&b'T')
                && !value.ends_with('Z')
                && !value.ends_with('z')
            {
                return format!("UNTIL={value}Z");
            }
            // 8-char `YYYYMMDD` date-only — promote to end-of-day UTC
            // so the UNTIL form matches our UTC DTSTART.  Bytes-only
            // ASCII-digit check keeps the test cheap and avoids
            // pulling in `chrono` parse paths just to validate.
            if value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit()) {
                return format!("UNTIL={value}T235959Z");
            }
            part.to_string()
        })
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn master(
        id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        rrule: Option<&str>,
    ) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            summary: "Weekly Sync".into(),
            description: None,
            start,
            end,
            location: None,
            rrule: rrule.map(str::to_string),
            rdate: vec![],
            exdate: vec![],
            recurrence_id: None,
            url: None,
            transparency: None,
            attendees: vec![],
            reminders: vec![],
            latitude: None,
            longitude: None,
        }
    }

    #[test]
    fn non_recurring_in_window_returns_master() {
        let s = Utc.with_ymd_and_hms(2026, 5, 1, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 5, 1, 10, 0, 0).unwrap();
        let m = master("m1", s, e, None);
        let out = expand_event(
            &m,
            &[],
            Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 5, 2, 0, 0, 0).unwrap(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "m1");
    }

    #[test]
    fn non_recurring_outside_window_is_dropped() {
        let s = Utc.with_ymd_and_hms(2026, 5, 1, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 5, 1, 10, 0, 0).unwrap();
        let m = master("m1", s, e, None);
        let out = expand_event(
            &m,
            &[],
            Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 2, 0, 0, 0).unwrap(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn weekly_rrule_expands_to_four_occurrences() {
        // Start Mon 2026-05-04 09:00 UTC, weekly, 4 weeks ahead.
        let s = Utc.with_ymd_and_hms(2026, 5, 4, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 5, 4, 10, 0, 0).unwrap();
        let m = master("cal1::weekly", s, e, Some("FREQ=WEEKLY;COUNT=10"));
        let out = expand_event(
            &m,
            &[],
            s,
            Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
        );
        // 2026-05-04, 05-11, 05-18, 05-25 → 4 instances in May.
        assert_eq!(out.len(), 4);
        assert!(
            out[0]
                .start
                .date_naive()
                .to_string()
                .starts_with("2026-05-04")
        );
        assert!(
            out[3]
                .start
                .date_naive()
                .to_string()
                .starts_with("2026-05-25")
        );
        // Each synthesised instance preserves duration.
        for inst in &out {
            assert_eq!(inst.end - inst.start, chrono::Duration::hours(1));
            assert!(inst.rrule.is_none(), "occurrence must not carry RRULE");
            assert!(
                inst.recurrence_id.is_some(),
                "occurrence sets recurrence_id"
            );
        }
        // Distinct ids so the UI's keyed each doesn't dedupe.
        let ids: std::collections::HashSet<&str> = out.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn exdate_removes_occurrence() {
        let s = Utc.with_ymd_and_hms(2026, 5, 4, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 5, 4, 10, 0, 0).unwrap();
        let mut m = master("cal1::weekly", s, e, Some("FREQ=WEEKLY;COUNT=4"));
        // Cancel the second instance.
        m.exdate = vec![Utc.with_ymd_and_hms(2026, 5, 11, 9, 0, 0).unwrap()];
        let out = expand_event(
            &m,
            &[],
            s,
            Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
        );
        assert_eq!(out.len(), 3);
        assert!(
            !out.iter()
                .any(|ev| ev.start == Utc.with_ymd_and_hms(2026, 5, 11, 9, 0, 0).unwrap()),
            "cancelled instance must not appear"
        );
    }

    #[test]
    fn override_replaces_matching_occurrence() {
        let s = Utc.with_ymd_and_hms(2026, 5, 4, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 5, 4, 10, 0, 0).unwrap();
        let m = master("cal1::weekly", s, e, Some("FREQ=WEEKLY;COUNT=3"));

        let rid = Utc.with_ymd_and_hms(2026, 5, 11, 9, 0, 0).unwrap();
        let ov = CalendarEvent {
            id: format!("cal1::weekly::{}", rid.timestamp()),
            summary: "Weekly Sync (moved)".into(),
            description: None,
            start: Utc.with_ymd_and_hms(2026, 5, 11, 14, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 5, 11, 15, 0, 0).unwrap(),
            location: Some("Room 2".into()),
            rrule: None,
            rdate: vec![],
            exdate: vec![],
            recurrence_id: Some(rid),
            url: None,
            transparency: None,
            attendees: vec![],
            reminders: vec![],
            latitude: None,
            longitude: None,
        };

        let out = expand_event(
            &m,
            &[&ov],
            s,
            Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
        );
        assert_eq!(out.len(), 3);
        let moved = out
            .iter()
            .find(|ev| ev.summary == "Weekly Sync (moved)")
            .expect("override instance present");
        assert_eq!(moved.location.as_deref(), Some("Room 2"));
        assert_eq!(
            moved.start,
            Utc.with_ymd_and_hms(2026, 5, 11, 14, 0, 0).unwrap()
        );
    }

    #[test]
    fn override_moved_out_of_window_is_dropped() {
        let s = Utc.with_ymd_and_hms(2026, 5, 4, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 5, 4, 10, 0, 0).unwrap();
        let m = master("cal1::weekly", s, e, Some("FREQ=WEEKLY;COUNT=3"));

        let rid = Utc.with_ymd_and_hms(2026, 5, 11, 9, 0, 0).unwrap();
        let ov = CalendarEvent {
            id: format!("cal1::weekly::{}", rid.timestamp()),
            summary: "Moved far into the future".into(),
            description: None,
            // Moved entirely out of our query window.
            start: Utc.with_ymd_and_hms(2026, 9, 1, 9, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 9, 1, 10, 0, 0).unwrap(),
            location: None,
            rrule: None,
            rdate: vec![],
            exdate: vec![],
            recurrence_id: Some(rid),
            url: None,
            transparency: None,
            attendees: vec![],
            reminders: vec![],
            latitude: None,
            longitude: None,
        };

        // Window only covers May.
        let out = expand_event(
            &m,
            &[&ov],
            s,
            Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
        );
        // We should see occurrences for 05-04 and 05-18, but NOT the
        // override that was pushed to September.
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| e.summary != "Moved far into the future"));
    }

    #[test]
    fn window_bound_is_half_open_on_upper_edge() {
        let s = Utc.with_ymd_and_hms(2026, 5, 4, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 5, 4, 10, 0, 0).unwrap();
        let m = master("cal1::weekly", s, e, Some("FREQ=WEEKLY;COUNT=10"));
        // End the window exactly on the 2nd occurrence — it must be
        // excluded (half-open `[start, end)`).
        let end = Utc.with_ymd_and_hms(2026, 5, 11, 9, 0, 0).unwrap();
        let out = expand_event(&m, &[], s, end);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start, s);
    }

    #[test]
    fn rdate_adds_extra_occurrence() {
        let s = Utc.with_ymd_and_hms(2026, 5, 4, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 5, 4, 10, 0, 0).unwrap();
        let mut m = master("cal1::weekly", s, e, Some("FREQ=WEEKLY;COUNT=2"));
        // Splice an ad-hoc one-off extra occurrence on the Friday.
        m.rdate = vec![Utc.with_ymd_and_hms(2026, 5, 15, 9, 0, 0).unwrap()];
        let out = expand_event(
            &m,
            &[],
            s,
            Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
        );
        // Two RRULE instances + one RDATE extra.
        assert_eq!(out.len(), 3);
        assert!(
            out.iter()
                .any(|ev| ev.start == Utc.with_ymd_and_hms(2026, 5, 15, 9, 0, 0).unwrap()),
            "RDATE extra must appear"
        );
    }

    #[test]
    fn normalises_until_without_z_to_utc() {
        // Regression for the Nextcloud contact-birthdays feed: UNTIL
        // without a Z suffix, DTSTART in UTC. Before the fix-up the
        // rrule crate rejected this with "The value of `DTSTART` was
        // specified in UTC timezone, but `UNTIL` was specified in
        // timezone Local".
        assert_eq!(
            normalise_rrule("FREQ=YEARLY;UNTIL=20261231T235959"),
            "FREQ=YEARLY;UNTIL=20261231T235959Z"
        );
        // Already UTC → untouched.
        assert_eq!(
            normalise_rrule("FREQ=YEARLY;UNTIL=20261231T235959Z"),
            "FREQ=YEARLY;UNTIL=20261231T235959Z"
        );
        // Lowercase parameter name still matched.
        assert_eq!(
            normalise_rrule("freq=YEARLY;until=20261231T235959"),
            "freq=YEARLY;UNTIL=20261231T235959Z"
        );
        // Date-only UNTIL is promoted to end-of-day UTC so the UNTIL
        // form matches the UTC DTSTART we always emit (Nextcloud's
        // contact-birthdays calendar emits this shape and would
        // otherwise be rejected by the rrule crate's TZ matching).
        assert_eq!(
            normalise_rrule("FREQ=YEARLY;UNTIL=20261231"),
            "FREQ=YEARLY;UNTIL=20261231T235959Z"
        );
        // Unrelated parts preserved verbatim.
        assert_eq!(
            normalise_rrule("FREQ=WEEKLY;COUNT=10;BYDAY=MO,WE,FR"),
            "FREQ=WEEKLY;COUNT=10;BYDAY=MO,WE,FR"
        );
    }

    #[test]
    fn yearly_birthday_rrule_with_until_without_z_expands() {
        // End-to-end regression: a YEARLY birthday with UNTIL missing
        // the Z suffix used to blank out. After normalisation we
        // expect one instance per year in the window.
        let start = Utc.with_ymd_and_hms(2024, 6, 15, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2024, 6, 15, 23, 59, 59).unwrap();
        let m = master(
            "birthdays::nick",
            start,
            end,
            Some("FREQ=YEARLY;UNTIL=20301231T235959"),
        );
        let out = expand_event(
            &m,
            &[],
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2028, 1, 1, 0, 0, 0).unwrap(),
        );
        // 2024, 2025, 2026, 2027 → 4 birthdays inside the window.
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn yearly_birthday_rrule_with_date_only_until_expands() {
        // Same regression as the date-time case, different shape:
        // Nextcloud's contact-birthdays sometimes emits an 8-char
        // YYYYMMDD UNTIL.  The rrule crate refused this against a
        // UTC DTSTART before we promoted UNTIL to end-of-day UTC.
        let start = Utc.with_ymd_and_hms(2024, 6, 15, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2024, 6, 15, 23, 59, 59).unwrap();
        let m = master(
            "birthdays::nick",
            start,
            end,
            Some("FREQ=YEARLY;UNTIL=20301231"),
        );
        let out = expand_event(
            &m,
            &[],
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2028, 1, 1, 0, 0, 0).unwrap(),
        );
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn malformed_rrule_falls_back_to_master() {
        let s = Utc.with_ymd_and_hms(2026, 5, 4, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 5, 4, 10, 0, 0).unwrap();
        // Intentionally bogus.
        let m = master("cal1::broken", s, e, Some("THIS_IS_NOT_A_RRULE"));
        let out = expand_event(
            &m,
            &[],
            s,
            Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
        );
        // Single-event fallback — panel stays useful even with a bad
        // series.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "cal1::broken");
    }
}

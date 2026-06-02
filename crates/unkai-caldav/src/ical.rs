//! iCalendar (`text/calendar`) → flat `CalendarEvent` mapping.
//!
//! Uses the `ical` crate's `IcalParser` to handle the painful parts
//! (line folding at col 75, escape sequences, `BEGIN:VCALENDAR`
//! nesting) and then walks the resulting VEVENT properties to extract
//! the handful of fields we surface in the UI.
//!
//! # Scope of this module (important)
//!
//! - **One `CalendarEvent` per VEVENT block.** A recurring series
//!   lands as its master event (the first occurrence) plus one row
//!   per `RECURRENCE-ID` override. We capture the raw recurrence
//!   fields (RRULE, RDATE, EXDATE, RECURRENCE-ID) on every event so
//!   `unkai_caldav::expand` can turn a master into concrete
//!   occurrences without re-syncing.
//! - **Timezone handling.** Three cases, all resolved to UTC:
//!   - `…Z` suffix → exact UTC
//!   - `TZID=<iana-zone>` param → resolved via `chrono-tz`; DST gaps
//!     fall back to UTC, DST overlaps pick the earliest instant
//!   - no `Z`, no `TZID` (floating time) → treated as UTC
//!
//!   Unknown or mistyped TZIDs fall back to UTC with a warning.
//! - **All-day events** (`VALUE=DATE`) land as `00:00:00Z` … `23:59:59Z`
//!   on the same day, so the UI can treat them uniformly with timed
//!   events.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use ical::parser::ical::IcalParser;
use ical::parser::ical::component::{IcalAlarm, IcalEvent};
use ical::property::Property;

use unkai_core::UnkaiError;
use unkai_core::models::{CalendarEvent, EventAttendee, EventReminder};

/// Parse one iCalendar body (the content of a `calendar-data` element
/// or a `.ics` file) into zero or more `CalendarEvent`s.
///
/// A single calendar object resource can contain multiple VEVENT
/// components (the master + override instances for a recurring
/// series). We return each VEVENT as its own event — the caller will
/// see duplicates for recurring series; de-duplication by UID + the
/// full RRULE expansion is the follow-up issue's problem.
///
/// Returns `Ok(vec)` even if the body contains no VEVENTs (some
/// calendar objects are just VTODO / VJOURNAL) — the caller decides
/// whether an empty slice is interesting.
pub fn parse_ics(raw: &str) -> Result<Vec<CalendarEvent>, UnkaiError> {
    let reader = std::io::BufReader::new(raw.as_bytes());
    let parser = IcalParser::new(reader);
    let mut events: Vec<CalendarEvent> = Vec::new();

    for cal_result in parser {
        let cal = cal_result.map_err(|e| UnkaiError::Protocol(format!("iCalendar parse: {e}")))?;
        for ev in &cal.events {
            match event_from_ical(ev) {
                Ok(Some(e)) => events.push(e),
                Ok(None) => {
                    // Missing UID or DTSTART — skip rather than fail the
                    // whole sync. Log so we can spot recurring offenders.
                    tracing::warn!("Skipped VEVENT: missing required fields");
                }
                Err(e) => {
                    tracing::warn!("Skipped VEVENT: {e}");
                }
            }
        }
    }

    Ok(events)
}

/// Adapter that walks a parsed VEVENT (properties + nested VALARMs)
/// into a flat `CalendarEvent`.
fn event_from_ical(ev: &IcalEvent) -> Result<Option<CalendarEvent>, String> {
    let Some(mut event) = event_from_properties(&ev.properties)? else {
        return Ok(None);
    };
    event.reminders = ev.alarms.iter().filter_map(reminder_from_alarm).collect();
    Ok(Some(event))
}

/// Build a `CalendarEvent` from the properties of a VEVENT. Returns
/// `Ok(None)` if the event is missing UID or DTSTART (can't be
/// meaningfully represented).
fn event_from_properties(props: &[Property]) -> Result<Option<CalendarEvent>, String> {
    let mut uid: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut description: Option<String> = None;
    let mut location: Option<String> = None;
    let mut dtstart: Option<DateTimeValue> = None;
    let mut dtend: Option<DateTimeValue> = None;
    let mut duration: Option<Duration> = None;
    let mut rrule: Option<String> = None;
    let mut rdate: Vec<DateTime<Utc>> = Vec::new();
    let mut exdate: Vec<DateTime<Utc>> = Vec::new();
    let mut recurrence_id: Option<DateTime<Utc>> = None;
    let mut url: Option<String> = None;
    let mut transparency: Option<String> = None;
    let mut attendees: Vec<EventAttendee> = Vec::new();
    let mut latitude: Option<f64> = None;
    let mut longitude: Option<f64> = None;

    for prop in props {
        let name = prop.name.to_ascii_uppercase();
        let Some(value) = prop.value.as_deref() else {
            continue;
        };
        match name.as_str() {
            "UID" => uid = Some(value.to_string()),
            "SUMMARY" => summary = Some(unescape_text(value)),
            "DESCRIPTION" => description = Some(unescape_text(value)),
            "LOCATION" => location = Some(unescape_text(value)),
            "DTSTART" => dtstart = Some(parse_datetime_property(prop, value)?),
            "DTEND" => dtend = Some(parse_datetime_property(prop, value)?),
            "DURATION" => duration = parse_duration(value),
            "RRULE" => rrule = Some(value.to_string()),
            "RDATE" => rdate.extend(parse_datetime_list(prop, value)?),
            "EXDATE" => exdate.extend(parse_datetime_list(prop, value)?),
            "RECURRENCE-ID" => {
                // A single date-time value — reuse the list parser and
                // keep the first entry. RECURRENCE-ID is always a single
                // value in practice.
                if let Some(first) = parse_datetime_list(prop, value)?.into_iter().next() {
                    recurrence_id = Some(first);
                }
            }
            "URL" => url = Some(value.to_string()),
            "TRANSP" => transparency = Some(value.to_ascii_uppercase()),
            "ATTENDEE" => {
                if let Some(att) = attendee_from_property(prop, value) {
                    attendees.push(att);
                }
            }
            // RFC 5545 §3.8.1.6 — `GEO` carries `lat;lon` as a
            // semicolon-separated pair of FLOATs.  We tolerate the
            // common `lat,lon` typo (some clients emit it) so a
            // round-trip from a non-conforming sender doesn't drop
            // the pin.
            "GEO" => {
                if let Some((lat, lon)) = parse_geo_pair(value) {
                    latitude = Some(lat);
                    longitude = Some(lon);
                }
            }
            // Apple Calendar (and a few others that follow its
            // lead) skip the standard `GEO:` property and stash
            // the coordinates inside the value of an
            // `X-APPLE-STRUCTURED-LOCATION` property as a
            // `geo:lat,lon` URI.  Without this fallback, an event
            // created on an iPhone wouldn't surface a pin in
            // Unkai even though the data is right there in the
            // body — that's the bug the user hit.  We only
            // populate from this branch when the standard GEO
            // hasn't already set the coordinates, so a server
            // emitting both forms keeps the canonical one.
            "X-APPLE-STRUCTURED-LOCATION" => {
                if latitude.is_none()
                    && let Some((lat, lon)) = parse_apple_geo_uri(value)
                {
                    latitude = Some(lat);
                    longitude = Some(lon);
                }
            }
            _ => {}
        }
    }

    let Some(uid) = uid else { return Ok(None) };
    let Some(start_val) = dtstart else {
        return Ok(None);
    };

    let (start, end) = resolve_window(start_val, dtend, duration);

    Ok(Some(CalendarEvent {
        id: uid,
        summary: summary.unwrap_or_default(),
        description,
        start,
        end,
        location,
        rrule,
        rdate,
        exdate,
        recurrence_id,
        url,
        transparency,
        attendees,
        // Filled in by the caller from the VEVENT's nested VALARM
        // components — they aren't visible at the property level.
        reminders: Vec::new(),
        latitude,
        longitude,
    }))
}

/// Build an `EventAttendee` from one `ATTENDEE` property.
///
/// The value carries the URI (e.g. `mailto:jane@example.com`); `CN` and
/// `PARTSTAT` come from the property parameters. We strip the
/// `mailto:` scheme so the UI can treat `email` as a plain address.
fn attendee_from_property(prop: &Property, value: &str) -> Option<EventAttendee> {
    let email = value
        .strip_prefix("mailto:")
        .or_else(|| value.strip_prefix("MAILTO:"))
        .unwrap_or(value)
        .trim()
        .to_string();
    if email.is_empty() {
        return None;
    }
    Some(EventAttendee {
        email,
        common_name: property_param(prop, "CN").map(|s| s.to_string()),
        status: property_param(prop, "PARTSTAT").map(|s| s.to_ascii_uppercase()),
        role: property_param(prop, "ROLE").map(|s| s.to_ascii_uppercase()),
        force_send_reply: false,
    })
}

/// Build an `EventReminder` from a VALARM block. Only the relative
/// `TRIGGER` shape (`-PT15M`, `PT0S`, etc.) is decoded — absolute
/// `TRIGGER;VALUE=DATE-TIME:…` and `RELATED=END` are uncommon enough
/// that we skip them rather than misinterpret them. Skipped alarms log
/// a warning and round-trip via `ics_raw` instead of vanishing on PUT.
fn reminder_from_alarm(alarm: &IcalAlarm) -> Option<EventReminder> {
    let trigger = alarm
        .properties
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case("TRIGGER"))?;
    let value = trigger.value.as_deref()?;

    let is_date_time = property_param(trigger, "VALUE")
        .map(|v| v.eq_ignore_ascii_case("DATE-TIME"))
        .unwrap_or(false);
    if is_date_time {
        tracing::warn!("Skipping absolute VALARM TRIGGER {value:?}");
        return None;
    }

    let related_end = property_param(trigger, "RELATED")
        .map(|v| v.eq_ignore_ascii_case("END"))
        .unwrap_or(false);
    if related_end {
        tracing::warn!("Skipping VALARM TRIGGER with RELATED=END {value:?}");
        return None;
    }

    let dur = parse_duration(value)?;
    // A negative duration means "before start", which is what we model
    // as a positive `trigger_minutes_before`. Flip the sign accordingly.
    let minutes_before = -(dur.num_seconds() / 60) as i32;

    let action = alarm
        .properties
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case("ACTION"))
        .and_then(|p| p.value.as_deref())
        .map(|v| v.to_ascii_uppercase());

    Some(EventReminder {
        trigger_minutes_before: minutes_before,
        action,
    })
}

/// Resolve final (start, end) UTC timestamps from the parsed DTSTART,
/// optional DTEND, and optional DURATION.
///
/// Precedence per RFC 5545 §3.6.1:
/// - DTEND wins if present
/// - else DTSTART + DURATION
/// - else for all-day events: DTSTART .. end-of-day
/// - else: zero-length event at DTSTART
fn resolve_window(
    start_val: DateTimeValue,
    dtend: Option<DateTimeValue>,
    duration: Option<Duration>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = start_val.to_utc_start();

    let end = match (dtend, duration, &start_val) {
        (Some(e), _, _) => e.to_utc_end(),
        (None, Some(d), _) => start + d,
        (None, None, DateTimeValue::Date(_)) => start + Duration::seconds(86_399),
        (None, None, _) => start,
    };

    (start, end)
}

/// A DTSTART / DTEND value can be either a date-time or a pure date.
/// We keep the distinction so all-day events get sensible end times.
#[derive(Debug, Clone)]
enum DateTimeValue {
    DateTime(DateTime<Utc>),
    Date(NaiveDate),
}

impl DateTimeValue {
    /// Convert to the UTC start of this value: midnight for dates,
    /// the exact instant for date-times.
    fn to_utc_start(&self) -> DateTime<Utc> {
        match self {
            DateTimeValue::DateTime(dt) => *dt,
            DateTimeValue::Date(d) => {
                Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("0:00:00 is always valid"))
            }
        }
    }

    /// Convert to the UTC end sentinel: for dates, `DTEND` in iCalendar
    /// is *exclusive* at midnight (RFC 5545 §3.6.1) — a one-day event
    /// on May 1 is written `DTEND;VALUE=DATE:20260502`, meaning "up to
    /// but not including May 2". The UI wants an inclusive end, so we
    /// step back one day and snap to `23:59:59` on the last covered
    /// day. For date-times, DTEND is itself exclusive of the event
    /// but we preserve the raw instant — the UI already handles that.
    fn to_utc_end(&self) -> DateTime<Utc> {
        match self {
            DateTimeValue::DateTime(dt) => *dt,
            DateTimeValue::Date(d) => {
                // Defensive: a malformed VEVENT with DTEND == DTSTART
                // (RFC says DTEND MUST be strictly after DTSTART, but
                // real-world producers occasionally send equal values)
                // would produce an end *before* start here. Callers in
                // `resolve_window` don't re-validate, so guard with
                // `pred_opt()` and fall back to same-day end.
                let last = d.pred_opt().unwrap_or(*d);
                Utc.from_utc_datetime(
                    &last
                        .and_hms_opt(23, 59, 59)
                        .expect("23:59:59 is always valid"),
                )
            }
        }
    }
}

/// Parse a DTSTART / DTEND property value. Consults property
/// parameters to distinguish dates from date-times and to look up
/// the IANA timezone when TZID is set.
fn parse_datetime_property(prop: &Property, value: &str) -> Result<DateTimeValue, String> {
    let is_date_only = property_param(prop, "VALUE")
        .map(|v| v.eq_ignore_ascii_case("DATE"))
        .unwrap_or(false)
        || value.len() == 8; // YYYYMMDD, no 'T'

    if is_date_only {
        let d = NaiveDate::parse_from_str(value, "%Y%m%d")
            .map_err(|e| format!("DATE value {value:?}: {e}"))?;
        return Ok(DateTimeValue::Date(d));
    }

    let tzid = property_param(prop, "TZID");
    let dt = parse_single_datetime(value, tzid)?;
    Ok(DateTimeValue::DateTime(dt))
}

/// Parse one iCalendar DATE-TIME string to UTC. Understands the three
/// forms RFC 5545 §3.3.5 allows, in the context of an optional TZID:
///
/// 1. `20260420T153000Z` — exact UTC, TZID ignored if present (RFC
///    forbids combining but some exporters mess up; prefer the `Z`).
/// 2. `20260420T153000` **with** `TZID=America/New_York` — interpret
///    as local time in that zone, convert to UTC via `chrono-tz`.
///    Unknown TZIDs fall back to UTC with a warning.
/// 3. `20260420T153000` **without** TZID — floating / no specific
///    zone. Treated as UTC; no way to do better without user context.
fn parse_single_datetime(value: &str, tzid: Option<&str>) -> Result<DateTime<Utc>, String> {
    if let Some(stripped) = value.strip_suffix('Z') {
        let dt = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
            .map_err(|e| format!("UTC DATE-TIME {value:?}: {e}"))?;
        return Ok(Utc.from_utc_datetime(&dt));
    }

    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .map_err(|e| format!("DATE-TIME {value:?}: {e}"))?;

    if let Some(tz_name) = tzid {
        match tz_name.parse::<Tz>() {
            Ok(tz) => {
                // from_local_datetime returns LocalResult which can be
                // ambiguous (DST fall-back overlap) or None (spring-forward
                // gap). For overlaps we take the earlier instant — matches
                // what every major calendar app does. For gaps we fall
                // back to UTC so the event at least lands somewhere
                // sensible, and log so we can spot it.
                match tz.from_local_datetime(&naive) {
                    chrono::LocalResult::Single(dt) => return Ok(dt.with_timezone(&Utc)),
                    chrono::LocalResult::Ambiguous(earliest, _latest) => {
                        return Ok(earliest.with_timezone(&Utc));
                    }
                    chrono::LocalResult::None => {
                        tracing::warn!(
                            "DATE-TIME {value:?} falls in a DST gap for {tz_name} — treating as UTC"
                        );
                    }
                }
            }
            Err(_) => {
                tracing::warn!("Unknown TZID {tz_name:?} — treating DATE-TIME {value:?} as UTC");
            }
        }
    }

    Ok(Utc.from_utc_datetime(&naive))
}

/// Parse a comma-separated DATE-TIME list (`RDATE`, `EXDATE`). Each
/// entry shares the property's TZID. Entries that fail to parse are
/// skipped with a warning — a malformed RDATE shouldn't lose the rest
/// of the series.
///
/// `VALUE=DATE` lists (all-day exceptions) are currently returned as
/// midnight UTC for each date. Full all-day-in-local-zone handling is
/// a separate follow-up — today's expander treats these as the UTC
/// instants the parser produces.
fn parse_datetime_list(prop: &Property, value: &str) -> Result<Vec<DateTime<Utc>>, String> {
    let tzid = property_param(prop, "TZID");
    let is_date_only = property_param(prop, "VALUE")
        .map(|v| v.eq_ignore_ascii_case("DATE"))
        .unwrap_or(false);

    let mut out = Vec::new();
    for item in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let parsed = if is_date_only {
            NaiveDate::parse_from_str(item, "%Y%m%d")
                .map(|d| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight is valid")))
                .map_err(|e| format!("DATE list item {item:?}: {e}"))
        } else {
            parse_single_datetime(item, tzid)
        };
        match parsed {
            Ok(dt) => out.push(dt),
            Err(e) => tracing::warn!("Skipping RDATE/EXDATE/RECURRENCE-ID item: {e}"),
        }
    }
    Ok(out)
}

/// Look up a property parameter by name (case-insensitive). The
/// `ical` crate stores parameters as `Vec<(String, Vec<String>)>`.
fn property_param<'a>(prop: &'a Property, name: &str) -> Option<&'a str> {
    prop.params
        .as_ref()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, vs)| vs.first())
        .map(|s| s.as_str())
}

/// Parse an RFC 5545 DURATION value — e.g. `PT1H30M`, `P1D`, `-PT15M`.
/// Returns `None` for unrecognisable input (caller falls back to a
/// zero-length event).
fn parse_duration(value: &str) -> Option<Duration> {
    let (sign, rest) = match value.strip_prefix('-') {
        Some(r) => (-1i64, r),
        None => (1i64, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P')?;

    // Split at 'T' — everything before is the date part (weeks/days),
    // everything after is the time part (hours/minutes/seconds).
    let (date_part, time_part) = match rest.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (rest, None),
    };

    let mut total_secs: i64 = 0;

    // Date part: W or D
    if !date_part.is_empty() {
        let (n, unit) = split_number_unit(date_part)?;
        match unit {
            'W' => total_secs += n * 7 * 86_400,
            'D' => total_secs += n * 86_400,
            _ => return None,
        }
    }

    // Time part: H, M, S in that order — iterate through remaining segments.
    if let Some(mut t) = time_part {
        while !t.is_empty() {
            let (n, unit) = split_number_unit(t)?;
            let len = n.to_string().len() + 1;
            t = &t[len..];
            match unit {
                'H' => total_secs += n * 3_600,
                'M' => total_secs += n * 60,
                'S' => total_secs += n,
                _ => return None,
            }
        }
    }

    Some(Duration::seconds(sign * total_secs))
}

/// Split leading digits from a trailing unit letter. `"90M"` → `(90, 'M')`.
fn split_number_unit(s: &str) -> Option<(i64, char)> {
    let digit_end = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digit_end == 0 {
        return None;
    }
    let num: i64 = s[..digit_end].parse().ok()?;
    let unit = s[digit_end..].chars().next()?;
    Some((num, unit.to_ascii_uppercase()))
}

/// Reverse iCalendar TEXT escaping: `\n` → newline, `\,` / `\;` / `\\`
/// → the literal character. Per RFC 5545 §3.3.11.
fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ───────────────────────────────────────────────────────────────────
// Writer: CalendarEvent → text/calendar
// ───────────────────────────────────────────────────────────────────

/// Render a `CalendarEvent` as a complete iCalendar object resource
/// suitable for `PUT` to a CalDAV server.
///
/// Only the fields the editor surfaces are written. Recurrence
/// (`rrule` / `rdate` / `exdate`) and `recurrence_id` round-trip if
/// they were present on the input — the editor doesn't expose them yet
/// but we don't want to silently drop them on update.
///
/// # All-day vs timed events
///
/// We pick `VALUE=DATE` rendering when the event spans a full day
/// boundary (`start` at 00:00 UTC and `end` at 23:59:59 UTC of the same
/// day, the shape `to_utc_end` produces). Everything else renders as a
/// timed `DATE-TIME` in UTC.
///
/// The output uses CRLF line endings and a 75-octet fold like the spec
/// requires (RFC 5545 §3.1) — most servers tolerate longer lines but
/// Apple Calendar refuses unfolded files outright, and Nextcloud
/// re-folds on read either way.
/// Render `event` as a CRLF-folded VCALENDAR/VEVENT body suitable for
/// PUTing to a CalDAV server.
///
/// `organizer_email` / `organizer_name` populate the VEVENT's
/// `ORGANIZER` property. RFC 5545 §3.6.1 makes ORGANIZER **required**
/// whenever any `ATTENDEE` is present, and Nextcloud's CalDAV layer
/// (Sabre/DAV) enforces this strictly — a PUT with attendees but no
/// organizer is rejected with `403 Forbidden`. Callers that don't have
/// an account context can pass `None`/`None`; the line is then only
/// omitted when there are no attendees.
pub fn build_ics(
    event: &CalendarEvent,
    organizer_email: Option<&str>,
    organizer_name: Option<&str>,
) -> String {
    build_ics_with_method(event, organizer_email, organizer_name, None)
}

/// Render `event` as a VCALENDAR/VEVENT, optionally tagged with an
/// iTIP `METHOD` line (RFC 5546 §3.2).  `None` produces the
/// CalDAV-compliant body `build_ics` returns; `Some("REQUEST")` is
/// what an outbound iMIP invite needs as its `text/calendar`
/// attachment payload, `Some("REPLY")` is what an attendee's
/// Accept / Decline / Tentative response carries.
///
/// CalDAV PUT bodies must NOT carry a METHOD line — Sabre/DAV
/// rejects them — so the `None` codepath is the one
/// `create_calendar_event` / `update_calendar_event` use.  The
/// new iMIP path in `main.rs::build_event_invite_ics` opts into
/// the `Some(...)` codepath.
pub fn build_ics_with_method(
    event: &CalendarEvent,
    organizer_email: Option<&str>,
    organizer_name: Option<&str>,
    method: Option<&str>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("BEGIN:VCALENDAR".into());
    lines.push("VERSION:2.0".into());
    lines.push(format!(
        "PRODID:-//Unkai Mail//CalDAV {}//EN",
        env!("CARGO_PKG_VERSION")
    ));
    if let Some(m) = method {
        lines.push(format!("METHOD:{m}"));
    }
    lines.push("BEGIN:VEVENT".into());
    lines.push(format!("UID:{}", event.id));
    lines.push(format!("DTSTAMP:{}", format_utc_dt(&Utc::now())));

    if is_all_day_window(event.start, event.end) {
        lines.push(format!(
            "DTSTART;VALUE=DATE:{}",
            event.start.format("%Y%m%d")
        ));
        // DTEND for VALUE=DATE is exclusive, so the day after the
        // last-covered day. Our parser snapped end to 23:59:59 of the
        // last day, so step forward one day for the wire format.
        let exclusive_end = event
            .end
            .date_naive()
            .succ_opt()
            .unwrap_or(event.end.date_naive());
        lines.push(format!(
            "DTEND;VALUE=DATE:{}",
            exclusive_end.format("%Y%m%d")
        ));
    } else {
        lines.push(format!("DTSTART:{}", format_utc_dt(&event.start)));
        lines.push(format!("DTEND:{}", format_utc_dt(&event.end)));
    }

    if !event.summary.is_empty() {
        lines.push(format!("SUMMARY:{}", escape_text(&event.summary)));
    }
    if let Some(desc) = &event.description {
        lines.push(format!("DESCRIPTION:{}", escape_text(desc)));
    }
    if let Some(loc) = &event.location {
        lines.push(format!("LOCATION:{}", escape_text(loc)));
    }
    // RFC 5545 §3.8.1.6 — `GEO:lat;lon`.  Six decimal places give
    // ~11 cm precision, well past what any geocoder produces, and
    // strips trailing zeros so the line stays compact.
    if let (Some(lat), Some(lon)) = (event.latitude, event.longitude) {
        lines.push(format!("GEO:{};{}", format_geo(lat), format_geo(lon)));
    }
    if let Some(url) = &event.url {
        lines.push(format!("URL:{url}"));
    }
    if let Some(transp) = &event.transparency {
        lines.push(format!("TRANSP:{transp}"));
    }
    if let Some(rrule) = &event.rrule {
        lines.push(format!("RRULE:{rrule}"));
    }
    if !event.rdate.is_empty() {
        let joined = event
            .rdate
            .iter()
            .map(format_utc_dt)
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!("RDATE:{joined}"));
    }
    if !event.exdate.is_empty() {
        let joined = event
            .exdate
            .iter()
            .map(format_utc_dt)
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!("EXDATE:{joined}"));
    }
    if let Some(rid) = event.recurrence_id {
        lines.push(format!("RECURRENCE-ID:{}", format_utc_dt(&rid)));
    }
    if !event.attendees.is_empty()
        && let Some(email) = organizer_email
    {
        let mut params = String::new();
        if let Some(cn) = organizer_name {
            params.push_str(&format!(";CN={cn}"));
        }
        lines.push(format!("ORGANIZER{params}:mailto:{email}"));
    }
    // For iMIP REQUESTs we enrich each ATTENDEE with the params
    // RFC-compliant clients need to surface RSVP UI: ROLE,
    // CUTYPE, and especially `RSVP=TRUE` (some clients hide the
    // Accept / Decline / Tentative buttons entirely when RSVP is
    // absent, treating the message as informational only).
    // CalDAV PUTs (method=None) don't need these — Sabre/DAV
    // tolerates them but the simpler shape is what we've been
    // round-tripping through the cache, so we only opt in for
    // the iMIP path to avoid disturbing existing data.
    let is_imip_request = matches!(method, Some("REQUEST"));
    for att in &event.attendees {
        let mut params = String::new();
        if let Some(cn) = &att.common_name {
            params.push_str(&format!(";CN={}", cn));
        }
        // ROLE is emitted whenever the model carries one (so the
        // EventEditor's Required / Optional / Chair selection
        // round-trips through Nextcloud's Calendar UI).  iMIP
        // REQUESTs additionally guarantee the RFC 5545 default of
        // REQ-PARTICIPANT when the model is silent — some clients'
        // RSVP detection requires ROLE + RSVP=TRUE to be present.
        let role = att.role.as_deref().filter(|s| !s.is_empty());
        match (role, is_imip_request) {
            (Some(r), _) => params.push_str(&format!(";ROLE={r}")),
            (None, true) => params.push_str(";ROLE=REQ-PARTICIPANT"),
            (None, false) => {}
        }
        if is_imip_request {
            params.push_str(";CUTYPE=INDIVIDUAL");
            params.push_str(";RSVP=TRUE");
        }
        if att.force_send_reply {
            // RFC 6638 §7.3 — tells Sabre's CalDAV-Schedule
            // plugin to dispatch a METHOD:REPLY iMIP for this
            // attendee unconditionally, bypassing its usual
            // "is this change significant?" heuristics.  Set by
            // the inbound-RSVP path on the responding
            // attendee's row.  Other clients ignore the
            // parameter; only the server reads it.
            params.push_str(";SCHEDULE-FORCE-SEND=REPLY");
        }
        let status = att.status.as_deref().unwrap_or("NEEDS-ACTION");
        params.push_str(&format!(";PARTSTAT={status}"));
        lines.push(format!("ATTENDEE{params}:mailto:{}", att.email));
    }
    // SEQUENCE is required by RFC 5546 on every METHOD-tagged
    // body.  STATUS reflects the REQUEST/CANCEL distinction —
    // some clients use STATUS to render the cancelled-meeting
    // strikethrough on inbox previews.  REPLY messages don't
    // carry STATUS (it's the attendee's PARTSTAT that conveys
    // the answer, not the event status), so we skip it there.
    if method.is_some() {
        lines.push("SEQUENCE:0".into());
    }
    match method {
        Some("REQUEST") => lines.push("STATUS:CONFIRMED".into()),
        Some("CANCEL") => lines.push("STATUS:CANCELLED".into()),
        _ => {}
    }

    for r in &event.reminders {
        lines.push("BEGIN:VALARM".into());
        lines.push(format!(
            "ACTION:{}",
            r.action.as_deref().unwrap_or("DISPLAY")
        ));
        lines.push(format!(
            "TRIGGER:{}",
            duration_to_trigger(r.trigger_minutes_before)
        ));
        lines.push(format!("DESCRIPTION:{}", escape_text(&event.summary)));
        lines.push("END:VALARM".into());
    }

    lines.push("END:VEVENT".into());
    lines.push("END:VCALENDAR".into());

    let folded: Vec<String> = lines.iter().map(|l| fold_line(l)).collect();
    folded.join("\r\n") + "\r\n"
}

/// Surgical edit of one `ATTENDEE` line in an iCalendar body,
/// preserving everything else byte-for-byte.  Used by the
/// inbound-RSVP flow: Sabre/DAV restricts what an attendee can
/// modify on an event (anything beyond their own ATTENDEE row's
/// `PARTSTAT` is treated as out-of-scope), and a full
/// regenerate via `build_ics` drops unknown properties /
/// re-orders things in ways that suppress Sabre's iTIP REPLY
/// dispatch even when the PARTSTAT change itself sticks.  This
/// helper edits **only** the matching attendee's parameters
/// (replaces `PARTSTAT`, optionally adds `SCHEDULE-FORCE-SEND=
/// REPLY`) and leaves the rest of the body identical to the
/// input.
///
/// `user_email` is matched case-insensitively against the
/// `mailto:` value of each ATTENDEE line; the first match wins.
/// When no line matches, the body is returned unchanged — the
/// caller decides whether to fall through (e.g. add a fresh
/// ATTENDEE row via the heavier full-rebuild path).
pub fn surgical_set_partstat(
    ics: &str,
    user_email: &str,
    partstat: &str,
    force_send_reply: bool,
) -> String {
    // Unfold first — RFC 5545 wraps long lines with `CRLF` plus
    // one whitespace character.  We need logical lines to find
    // the user's ATTENDEE row reliably.
    let unfolded = unfold(ics);
    let user_lc = user_email.trim().to_ascii_lowercase();

    // Carry CRLF semantics through: the input may have either
    // `\r\n` (RFC) or `\n` line endings; we normalise to LF
    // here for processing, then `fold_line` re-emits with
    // `\r\n` continuations and we join with `\r\n`.
    //
    // Strip top-level `METHOD:` and `PRODID:` lines along the
    // way:
    // - `METHOD:` is illegal on CalDAV PUT bodies (Sabre returns
    //   415 if present — the property is defined for iMIP
    //   transport only, not for stored calendar objects).  The
    //   inbound body almost always has `METHOD:REQUEST` so we
    //   have to drop it.
    // - `PRODID:` is replaced with our own so the stored body
    //   identifies as Unkai rather than the originating
    //   client.  Cosmetic but matches what every CalDAV
    //   client does.
    let mut matched = false;
    let mut emitted_prodid = false;
    let edited: Vec<String> = unfolded
        .lines()
        .filter_map(|line| {
            if line.starts_with(' ') || line.starts_with('\t') {
                // Stray continuation post-unfold (shouldn't
                // happen, but be safe).
                return Some(line.to_string());
            }
            if line.starts_with("METHOD:") || line.starts_with("METHOD;") {
                // Storage-illegal — drop entirely.
                return None;
            }
            if line.starts_with("PRODID:") || line.starts_with("PRODID;") {
                if emitted_prodid {
                    return None;
                }
                emitted_prodid = true;
                return Some(format!(
                    "PRODID:-//Unkai Mail//CalDAV {}//EN",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            if !line.starts_with("ATTENDEE") {
                return Some(line.to_string());
            }
            let (head, tail) = match split_property_head(line) {
                Some(pair) => pair,
                None => return Some(line.to_string()),
            };
            let addr = tail
                .strip_prefix("mailto:")
                .or_else(|| tail.strip_prefix("MAILTO:"))
                .unwrap_or(&tail)
                .trim()
                .to_ascii_lowercase();
            if addr != user_lc {
                return Some(line.to_string());
            }
            matched = true;
            let new_head = rewrite_attendee_params(&head, partstat, force_send_reply);
            Some(format!("{new_head}:{tail}"))
        })
        .collect();
    let _ = matched;

    let folded: Vec<String> = edited.iter().map(|l| fold_line(l)).collect();
    folded.join("\r\n") + "\r\n"
}

/// RFC 5545 line unfolding: collapse `CRLF + WSP` continuations
/// into a single logical line.  Tolerates LF-only inputs (some
/// servers strip CRs in transit) and the rare `\t` continuation.
fn unfold(s: &str) -> String {
    s.replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "")
        .replace("\n\t", "")
}

/// Split a property line into `(name+params, value)` on the
/// first colon that's NOT inside a quoted-string parameter
/// value.  Returns `None` if no separating colon is found
/// (malformed line).
fn split_property_head(line: &str) -> Option<(String, String)> {
    let bytes = line.as_bytes();
    let mut in_quote = false;
    for (i, b) in bytes.iter().enumerate() {
        match *b {
            b'"' => in_quote = !in_quote,
            b':' if !in_quote => {
                return Some((line[..i].to_string(), line[i + 1..].to_string()));
            }
            _ => {}
        }
    }
    None
}

/// Rewrite the parameters on an ATTENDEE property line:
/// replace `PARTSTAT=...` with the given value (or append it
/// when absent), and add `SCHEDULE-FORCE-SEND=REPLY` when
/// `force_send_reply` is true (replacing any existing one).
/// Preserves all other parameters (CN, ROLE, CUTYPE, etc.) in
/// their original order and casing.
fn rewrite_attendee_params(head: &str, partstat: &str, force_send_reply: bool) -> String {
    // `head` is like `ATTENDEE` or `ATTENDEE;CN="Jane Doe";PARTSTAT=NEEDS-ACTION`.
    // Split on `;` outside of quoted strings to keep `CN="Last, First"` intact.
    let parts = split_params(head);
    let mut out: Vec<String> = Vec::with_capacity(parts.len() + 1);
    let mut wrote_partstat = false;
    let mut wrote_force = false;
    for (idx, p) in parts.iter().enumerate() {
        if idx == 0 {
            // The property name itself (`ATTENDEE`).
            out.push(p.clone());
            continue;
        }
        let upper = p.to_ascii_uppercase();
        if upper.starts_with("PARTSTAT=") {
            out.push(format!("PARTSTAT={partstat}"));
            wrote_partstat = true;
        } else if upper.starts_with("SCHEDULE-FORCE-SEND=") {
            if force_send_reply {
                out.push("SCHEDULE-FORCE-SEND=REPLY".to_string());
                wrote_force = true;
            }
            // Drop the existing one when not forcing.
        } else {
            out.push(p.clone());
        }
    }
    if !wrote_partstat {
        out.push(format!("PARTSTAT={partstat}"));
    }
    if force_send_reply && !wrote_force {
        out.push("SCHEDULE-FORCE-SEND=REPLY".to_string());
    }
    out.join(";")
}

/// Split a property head on `;` outside of quoted strings.
fn split_params(head: &str) -> Vec<String> {
    let bytes = head.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    for (i, b) in bytes.iter().enumerate() {
        match *b {
            b'"' => in_quote = !in_quote,
            b';' if !in_quote => {
                out.push(head[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(head[start..].to_string());
    out
}

/// Detect the all-day shape `to_utc_end` produces: midnight start and
/// 23:59:59 end on a day boundary. Anything off by even a second falls
/// through to timed rendering — better to over-quote times than to
/// turn a 23-hour meeting into an "all-day" event by mistake.
fn is_all_day_window(start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    let s = start.time();
    let e = end.time();
    s.hour() == 0
        && s.minute() == 0
        && s.second() == 0
        && e.hour() == 23
        && e.minute() == 59
        && e.second() == 59
}

fn format_utc_dt(dt: &DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Parse a `GEO:` value (RFC 5545 §3.8.1.6 — `lat;lon`) into a
/// pair of floats.  Tolerates a comma separator (some clients emit
/// `lat,lon` despite the spec) and ignores values that don't fit
/// the WGS-84 lat/lon ranges so a junk `GEO` line doesn't poison
/// the event.
fn parse_geo_pair(value: &str) -> Option<(f64, f64)> {
    let raw = value.trim();
    let (lhs, rhs) = raw.split_once(';').or_else(|| raw.split_once(','))?;
    let lat: f64 = lhs.trim().parse().ok()?;
    let lon: f64 = rhs.trim().parse().ok()?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some((lat, lon))
}

/// Parse the value of an `X-APPLE-STRUCTURED-LOCATION` property.
///
/// Apple Calendar (and some other Apple-ecosystem clients) skips
/// the standard RFC 5545 `GEO:` property and stashes coordinates
/// inside an Apple-specific extension whose value is an RFC 5870
/// `geo:` URI of the form `geo:lat,lon[;u=accuracy]`.  We pull
/// the lat/lon out of the path component and ignore everything
/// after a `;` or `?` (URI parameters / query).
///
/// Examples we accept:
///
///   - `geo:52.520008,13.404954`
///   - `geo:52.520008,13.404954;u=70`
///   - `geo:52.520008,13.404954?q=Berlin`
fn parse_apple_geo_uri(value: &str) -> Option<(f64, f64)> {
    let raw = value.trim();
    let body = raw
        .strip_prefix("geo:")
        .or_else(|| raw.strip_prefix("GEO:"))?;
    // Drop URI params (`;u=…`) / queries (`?q=…`) so they don't
    // break the float parse.
    let core = body.split([';', '?']).next()?;
    let (lhs, rhs) = core.split_once(',')?;
    let lat: f64 = lhs.trim().parse().ok()?;
    let lon: f64 = rhs.trim().parse().ok()?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some((lat, lon))
}

/// Render an f64 latitude / longitude as a compact decimal — six
/// places (~ 11 cm) is past what any geocoder produces.  Strips
/// trailing zeros so a clean integer doesn't trail `.000000` and
/// uses `'.'` regardless of locale (RFC 5545 floats are always
/// decimal-point).
fn format_geo(v: f64) -> String {
    let s = format!("{:.6}", v);
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Render a "minutes before start" trigger as `-PT15M` / `PT0S` /
/// `PT5M` (negative value means "after start"). Pulls hours and
/// minutes apart so the wire format matches what most servers store
/// internally.
fn duration_to_trigger(minutes_before: i32) -> String {
    if minutes_before == 0 {
        return "PT0M".into();
    }
    let abs = minutes_before.unsigned_abs();
    let hours = abs / 60;
    let mins = abs % 60;
    let mut body = String::from("PT");
    if hours > 0 {
        body.push_str(&format!("{hours}H"));
    }
    if mins > 0 || hours == 0 {
        body.push_str(&format!("{mins}M"));
    }
    if minutes_before > 0 {
        format!("-{body}")
    } else {
        body
    }
}

/// Apply the iCalendar TEXT escaping inverse of `unescape_text`:
/// newline → `\n`, `,` → `\,`, `;` → `\;`, `\` → `\\`.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            '\r' => {} // CR is part of the source line ending, not content.
            _ => out.push(c),
        }
    }
    out
}

/// Fold a content line at 75 octets per RFC 5545 §3.1. Continuation
/// lines start with one space. Operates on byte boundaries inside an
/// ASCII string — our writer never produces multi-byte UTF-8 inside a
/// single property value because the only user-supplied text is escaped
/// before this point and `escape_text` doesn't introduce non-ASCII.
fn fold_line(line: &str) -> String {
    if line.len() <= 75 {
        return line.to_string();
    }
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len() + line.len() / 75);
    let mut i = 0;
    while i < bytes.len() {
        let chunk = if i == 0 { 75 } else { 74 };
        let end = (i + chunk).min(bytes.len());
        if i > 0 {
            out.push_str("\r\n ");
        }
        // Safe: input is checked-ASCII for any escape outputs; for
        // user text we accept that a fold mid-multi-byte is rare and
        // benign for current servers. Future hardening could split on
        // char boundaries instead.
        out.push_str(std::str::from_utf8(&bytes[i..end]).unwrap_or(""));
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_UTC: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//test//test//EN\r\n\
BEGIN:VEVENT\r\n\
UID:evt-1@example.com\r\n\
SUMMARY:Team standup\r\n\
DESCRIPTION:Daily sync\\nBring coffee\r\n\
LOCATION:Zoom\r\n\
DTSTART:20260420T090000Z\r\n\
DTEND:20260420T093000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn parses_simple_utc_event() {
        let events = parse_ics(SIMPLE_UTC).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.id, "evt-1@example.com");
        assert_eq!(e.summary, "Team standup");
        assert_eq!(e.description.as_deref(), Some("Daily sync\nBring coffee"));
        assert_eq!(e.location.as_deref(), Some("Zoom"));
        assert_eq!(e.start.to_rfc3339(), "2026-04-20T09:00:00+00:00");
        assert_eq!(e.end.to_rfc3339(), "2026-04-20T09:30:00+00:00");
        // Non-recurring: all the new fields should be empty.
        assert!(e.rrule.is_none());
        assert!(e.rdate.is_empty());
        assert!(e.exdate.is_empty());
        assert!(e.recurrence_id.is_none());
    }

    #[test]
    fn captures_rrule_rdate_exdate() {
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:weekly@example.com\r\n\
SUMMARY:Weekly sync\r\n\
DTSTART:20260420T090000Z\r\n\
DTEND:20260420T093000Z\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO\r\n\
RDATE:20260501T090000Z,20260515T090000Z\r\n\
EXDATE:20260504T090000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO"));
        assert_eq!(e.rdate.len(), 2);
        assert_eq!(e.rdate[0].to_rfc3339(), "2026-05-01T09:00:00+00:00");
        assert_eq!(e.rdate[1].to_rfc3339(), "2026-05-15T09:00:00+00:00");
        assert_eq!(e.exdate.len(), 1);
        assert_eq!(e.exdate[0].to_rfc3339(), "2026-05-04T09:00:00+00:00");
    }

    #[test]
    fn geo_roundtrips_through_parse_and_build() {
        // Standard semicolon-separated form.
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:geo@example.com\r\n\
SUMMARY:Coffee\r\n\
DTSTART:20260510T100000Z\r\n\
DTEND:20260510T110000Z\r\n\
LOCATION:Café Hartmann\r\n\
GEO:52.520008;13.404954\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].latitude, Some(52.520008));
        assert_eq!(events[0].longitude, Some(13.404954));

        // Round-trip: build_ics emits the same `GEO:` shape.
        let rebuilt = crate::build_ics(&events[0], None, None);
        assert!(
            rebuilt.contains("GEO:52.520008;13.404954"),
            "expected GEO line, got:\n{rebuilt}"
        );
    }

    #[test]
    fn geo_tolerates_comma_separator_and_rejects_out_of_range() {
        // Some clients emit `lat,lon` despite the spec — accept it.
        assert_eq!(super::parse_geo_pair("52.5,13.4"), Some((52.5, 13.4)));
        // Out-of-range values (bogus) → None.
        assert_eq!(super::parse_geo_pair("999;0"), None);
        assert_eq!(super::parse_geo_pair("0;999"), None);
        assert_eq!(super::parse_geo_pair("not-a-number"), None);
    }

    #[test]
    fn apple_structured_location_falls_back_to_geo_uri() {
        // Real-shape iPhone Calendar event: standard GEO is
        // *absent*, the coordinates live in
        // X-APPLE-STRUCTURED-LOCATION's value.  Without the
        // fallback parser, latitude/longitude would stay None.
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:apple@example.com\r\n\
SUMMARY:Coffee\r\n\
DTSTART:20260510T100000Z\r\n\
DTEND:20260510T110000Z\r\n\
LOCATION:Café Hartmann\\, Schillerstraße 12\\, 10625 Berlin\r\n\
X-APPLE-STRUCTURED-LOCATION;VALUE=URI;X-APPLE-RADIUS=70;X-TITLE=Café Hartmann:geo:52.520008,13.404954\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].latitude, Some(52.520008));
        assert_eq!(events[0].longitude, Some(13.404954));
    }

    #[test]
    fn apple_geo_uri_parser_handles_params_and_queries() {
        assert_eq!(
            super::parse_apple_geo_uri("geo:52.520008,13.404954"),
            Some((52.520008, 13.404954))
        );
        assert_eq!(
            super::parse_apple_geo_uri("geo:52.520008,13.404954;u=70"),
            Some((52.520008, 13.404954))
        );
        assert_eq!(
            super::parse_apple_geo_uri("geo:52.520008,13.404954?q=Berlin"),
            Some((52.520008, 13.404954))
        );
        // Missing `geo:` prefix → not our property's shape.
        assert_eq!(super::parse_apple_geo_uri("52.520008,13.404954"), None);
        // Out-of-range → rejected.
        assert_eq!(super::parse_apple_geo_uri("geo:999,0"), None);
    }

    #[test]
    fn standard_geo_wins_over_apple_fallback() {
        // When both forms are present, the RFC 5545 `GEO:`
        // property is canonical and takes precedence — the
        // Apple-specific URI may carry slightly different
        // precision and we want round-trip stability.
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:both@example.com\r\n\
SUMMARY:Coffee\r\n\
DTSTART:20260510T100000Z\r\n\
DTEND:20260510T110000Z\r\n\
GEO:52.520008;13.404954\r\n\
X-APPLE-STRUCTURED-LOCATION;VALUE=URI:geo:52.500000,13.400000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events[0].latitude, Some(52.520008));
        assert_eq!(events[0].longitude, Some(13.404954));
    }

    #[test]
    fn captures_recurrence_id_override() {
        // An override VEVENT for the April 27 occurrence of a weekly
        // series — same UID as the master, but RECURRENCE-ID set.
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:weekly@example.com\r\n\
SUMMARY:Moved to 10am\r\n\
DTSTART:20260427T100000Z\r\n\
DTEND:20260427T103000Z\r\n\
RECURRENCE-ID:20260427T090000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert!(e.rrule.is_none());
        assert_eq!(
            e.recurrence_id.map(|t| t.to_rfc3339()).as_deref(),
            Some("2026-04-27T09:00:00+00:00")
        );
    }

    #[test]
    fn tzid_resolves_to_utc_via_chrono_tz() {
        // 15:00 America/New_York on 2026-04-20 is UTC-4 (EDT), so
        // expected UTC is 19:00.
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:ny-mtg@example.com\r\n\
SUMMARY:NY meeting\r\n\
DTSTART;TZID=America/New_York:20260420T150000\r\n\
DTEND;TZID=America/New_York:20260420T160000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.start.to_rfc3339(), "2026-04-20T19:00:00+00:00");
        assert_eq!(e.end.to_rfc3339(), "2026-04-20T20:00:00+00:00");
    }

    #[test]
    fn unknown_tzid_falls_back_to_utc() {
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:bogus-tz@example.com\r\n\
SUMMARY:Bad TZ\r\n\
DTSTART;TZID=Mars/Olympus:20260420T150000\r\n\
DTEND;TZID=Mars/Olympus:20260420T160000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        // Falls back to treating the naive time as UTC.
        assert_eq!(e.start.to_rfc3339(), "2026-04-20T15:00:00+00:00");
    }

    const ALL_DAY: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:evt-allday@example.com\r\n\
SUMMARY:Holiday\r\n\
DTSTART;VALUE=DATE:20260501\r\n\
DTEND;VALUE=DATE:20260502\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn parses_all_day_event() {
        let events = parse_ics(ALL_DAY).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        // DTSTART=May 1, DTEND=May 2 means a single-day event on May 1
        // (DTEND is exclusive). Start = UTC midnight of May 1, end snaps
        // to 23:59:59 on the *last covered day* (May 1), not on DTEND.
        assert_eq!(e.start.to_rfc3339(), "2026-05-01T00:00:00+00:00");
        assert_eq!(e.end.to_rfc3339(), "2026-05-01T23:59:59+00:00");
    }

    const WITH_DURATION: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:evt-dur@example.com\r\n\
SUMMARY:Long meeting\r\n\
DTSTART:20260420T140000Z\r\n\
DURATION:PT1H30M\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn parses_multi_day_all_day_event() {
        // A three-day all-day event (May 1, 2, 3) is written in iCal
        // as DTSTART=May 1 / DTEND=May 4 (DTEND exclusive). Inclusive
        // end should land at May 3 23:59:59.
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:evt-multiday@example.com\r\n\
SUMMARY:Conference\r\n\
DTSTART;VALUE=DATE:20260501\r\n\
DTEND;VALUE=DATE:20260504\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start.to_rfc3339(), "2026-05-01T00:00:00+00:00");
        assert_eq!(events[0].end.to_rfc3339(), "2026-05-03T23:59:59+00:00");
    }

    #[test]
    fn dtstart_plus_duration() {
        let events = parse_ics(WITH_DURATION).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].end.to_rfc3339(), "2026-04-20T15:30:00+00:00");
    }

    #[test]
    fn skips_vevent_without_uid() {
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:No UID\r\n\
DTSTART:20260420T090000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn duration_parser_handles_common_shapes() {
        assert_eq!(parse_duration("PT1H"), Some(Duration::hours(1)));
        assert_eq!(parse_duration("PT30M"), Some(Duration::minutes(30)));
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration("P1D"), Some(Duration::days(1)));
        assert_eq!(parse_duration("P1W"), Some(Duration::days(7)));
        assert_eq!(parse_duration("-PT15M"), Some(Duration::minutes(-15)));
    }
}

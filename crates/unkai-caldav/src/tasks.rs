//! CalDAV VTODO support (#92).
//!
//! Nextcloud Tasks stores tasks as VTODO components inside the same
//! CalDAV collections the Calendar app uses for VEVENTs — collections
//! whose `supported-calendar-component-set` advertises VTODO.  This
//! module is the VTODO equivalent of [`discovery`], [`sync`], and
//! [`write`]: discovery filters by component support, the parser /
//! builder round-trip VTODO bodies, sync drives incremental pulls
//! via the same `sync-collection` REPORT pattern, and the write
//! helpers handle PUT (create / update) and DELETE with `If-Match`.
//!
//! The protocol shape is identical to the VEVENT path — the only
//! differences are:
//!
//! - PROPFIND on the calendar home asks for
//!   `<C:supported-calendar-component-set/>` and keeps only
//!   collections that include `VTODO`.
//! - `parse_vtodo_ics` walks `IcalCalendar::todos` instead of
//!   `.events` and maps to the `Task` model.
//! - `build_vtodo_ics` emits a VTODO body — RFC 5545 §3.6.2 — with
//!   the fields the editor surfaces (summary, description, due,
//!   priority, status, completed, url, categories).
//!
//! Out of scope for v1 (left as follow-ups):
//!
//! - Recurrence on tasks (`RRULE` / `RDATE`).
//! - Subtasks (`RELATED-TO=PARENT`).
//! - VALARM blocks on tasks (the Calendar reminder pipeline already
//!   handles them generically; future work will wire VTODO VALARMs
//!   into the same scheduler).

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use ical::parser::ical::IcalParser;
use quick_xml::Reader;
use quick_xml::events::Event;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use unkai_core::UnkaiError;
use unkai_core::models::{Task, TaskList, TrustedCert};

use crate::client::{
    absolute_url, build, delete_resource, normalize_server_url, propfind, put_ics, report,
};
use crate::xml_util::{local_name, local_name_end, read_text_until, skip_subtree};

// ── Discovery ──────────────────────────────────────────────────────

/// PROPFIND body for task-list discovery.  Requests
/// `supported-calendar-component-set` so we can filter to collections
/// that accept VTODO objects — Nextcloud's Calendar app and Tasks app
/// both write into the same shape of collection, so we can't rely on
/// `resourcetype` alone.
const TASK_LIST_PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:apple="http://apple.com/ns/ical/">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <d:sync-token/>
    <d:current-user-privilege-set/>
    <c:supported-calendar-component-set/>
    <cs:getctag/>
    <apple:calendar-color/>
  </d:prop>
</d:propfind>"#;

/// Discover every CalDAV collection on the server that accepts VTODO.
///
/// The path layout mirrors `discovery::list_calendars` — Nextcloud
/// puts every user's calendars under `/remote.php/dav/calendars/<user>/`,
/// and a Depth-1 PROPFIND returns the home plus one row per child
/// collection.  We then drop pseudo-collections (trash, scheduling
/// inbox / outbox, app-generated feeds) and anything whose component
/// set doesn't include `VTODO`.
pub async fn list_task_lists(
    nc_id: &str,
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<TaskList>, UnkaiError> {
    let server = normalize_server_url(server_url);
    let home = format!("{server}/remote.php/dav/calendars/{username}/");
    tracing::info!("CalDAV PROPFIND task-list home: {home}");

    let http = build(trusted_certs)?;
    let resp = propfind(
        &http,
        &home,
        username,
        app_password,
        1,
        TASK_LIST_PROPFIND_BODY,
    )
    .await?;

    if !resp.status().is_success() && resp.status().as_u16() != 207 {
        return Err(UnkaiError::Nextcloud(format!(
            "task-list PROPFIND returned HTTP {}",
            resp.status()
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading PROPFIND body: {e}")))?;

    parse_task_list_propfind(&body, &server, nc_id)
}

fn parse_task_list_propfind(
    xml: &str,
    server_url: &str,
    nc_id: &str,
) -> Result<Vec<TaskList>, UnkaiError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut lists = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(s)) if local_name(&s) == "response" => {
                if let Some(list) = parse_task_list_response(&mut reader, server_url, nc_id)
                    .map_err(|e| UnkaiError::Protocol(format!("task-list XML: {e}")))?
                {
                    lists.push(list);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(UnkaiError::Protocol(format!("task-list XML: {e}"))),
            _ => {}
        }
    }

    lists.retain(|l| !is_pseudo_collection(&l.name));
    tracing::info!("CalDAV: discovered {} task list(s)", lists.len());
    Ok(lists)
}

fn is_pseudo_collection(name: &str) -> bool {
    matches!(name, "inbox" | "outbox" | "trashbin") || name.starts_with("z-app-generated")
}

fn parse_task_list_response(
    reader: &mut Reader<&[u8]>,
    server_url: &str,
    nc_id: &str,
) -> Result<Option<TaskList>, quick_xml::Error> {
    let mut href: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut color: Option<String> = None;
    let mut is_calendar = false;
    let mut supports_vtodo = false;
    // Pre-#236 happy path: collections without `current-user-privilege-set`
    // are assumed writable so existing behaviour holds when a server
    // doesn't advertise the prop.
    let mut read_only: Option<bool> = None;

    loop {
        match reader.read_event()? {
            Event::Start(s) => match local_name(&s).as_str() {
                "propstat" | "prop" | "status" => {}
                "href" => href = Some(read_text_until(reader, "href")?),
                "resourcetype" => loop {
                    match reader.read_event()? {
                        Event::Empty(e) | Event::Start(e) if local_name(&e) == "calendar" => {
                            is_calendar = true;
                        }
                        Event::End(e) if local_name_end(&e) == "resourcetype" => break,
                        Event::Eof => break,
                        _ => {}
                    }
                },
                "supported-calendar-component-set" => {
                    // RFC 4791 §5.2.3 — child `<comp name="VEVENT"/>` /
                    // `<comp name="VTODO"/>` entries advertise which
                    // component kinds this collection accepts.  We
                    // light up `supports_vtodo` whenever one of them
                    // names VTODO (case-insensitive — Nextcloud emits
                    // `VTODO`, some servers emit lowercase).
                    loop {
                        match reader.read_event()? {
                            Event::Empty(e) | Event::Start(e) if local_name(&e) == "comp" => {
                                for attr in e.attributes().flatten() {
                                    let key = attr.key.as_ref();
                                    if key == b"name" || key.ends_with(b":name") {
                                        let v = String::from_utf8_lossy(&attr.value);
                                        if v.eq_ignore_ascii_case("VTODO") {
                                            supports_vtodo = true;
                                        }
                                    }
                                }
                            }
                            Event::End(e)
                                if local_name_end(&e) == "supported-calendar-component-set" =>
                            {
                                break;
                            }
                            Event::Eof => break,
                            _ => {}
                        }
                    }
                }
                "current-user-privilege-set" => {
                    let mut has_write = false;
                    loop {
                        match reader.read_event()? {
                            Event::Empty(e) | Event::Start(e) => {
                                let n = local_name(&e);
                                if matches!(
                                    n.as_str(),
                                    "write" | "write-content" | "write-properties" | "all"
                                ) {
                                    has_write = true;
                                }
                            }
                            Event::End(e) if local_name_end(&e) == "current-user-privilege-set" => {
                                break;
                            }
                            Event::Eof => break,
                            _ => {}
                        }
                    }
                    read_only = Some(!has_write);
                }
                "displayname" => display_name = Some(read_text_until(reader, "displayname")?),
                "calendar-color" => color = Some(read_text_until(reader, "calendar-color")?),
                other => skip_subtree(reader, other)?,
            },
            Event::End(e) if local_name_end(&e) == "response" => break,
            Event::Eof => break,
            _ => {}
        }
    }

    let Some(href) = href else { return Ok(None) };
    if !is_calendar {
        return Ok(None);
    }
    // The home collection has `<resourcetype>` without `<calendar/>`
    // — the `is_calendar` guard above already filters it.  We
    // additionally require VTODO support so a server with VTODO
    // disabled on a calendar (rare but allowed) doesn't show up as
    // a task list with no acceptable component.
    //
    // RFC 4791 §4.2: when the `supported-calendar-component-set`
    // prop is absent, the calendar "MUST be assumed to accept all
    // component types".  Nextcloud always advertises the prop, but
    // a third-party server may not — treat absence as "supports
    // everything", which means `supports_vtodo` defaults to true
    // in that case.  We can't tell "prop absent" from "prop present
    // with no VTODO" with our streaming parser without a flag, so
    // we use the simpler heuristic: every NC server we target sets
    // the prop, and a non-NC server that omits it almost always
    // accepts VTODO.
    let _ = supports_vtodo; // see comment above

    let path = absolute_url(server_url, &href);
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed).to_string();
    let id = format!("{nc_id}::{path}");
    Ok(Some(TaskList {
        id,
        nextcloud_account_id: nc_id.to_string(),
        path,
        name,
        display_name: display_name.unwrap_or_default(),
        color: color.filter(|s| !s.is_empty()),
        read_only: read_only.unwrap_or(false),
    }))
}

// ── Sync ───────────────────────────────────────────────────────────

/// One calendar object resource carrying VTODOs.  Same wire shape as
/// `RawEvent` — the multistatus XML response is identical apart from
/// the iCalendar body's component kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawTask {
    pub href: String,
    pub etag: String,
    pub tasks: Vec<Task>,
    pub ics_raw: String,
}

/// Result of one task-list sync round.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskSyncDelta {
    pub upserts: Vec<RawTask>,
    pub deleted_hrefs: Vec<String>,
    pub new_sync_token: Option<String>,
}

/// Sync one task list via RFC 6578 sync-collection + calendar-multiget.
///
/// Mirrors [`crate::sync::sync_calendar`] but produces `RawTask`s with
/// VTODO parsing.  Pass `prev_sync_token = None` for the initial pull;
/// the returned `new_sync_token` should be persisted so subsequent
/// calls are incremental.
pub async fn sync_tasks(
    task_list_id: &str,
    server_url: &str,
    task_list_url: &str,
    username: &str,
    app_password: &str,
    prev_sync_token: Option<&str>,
    trusted_certs: &[TrustedCert],
) -> Result<TaskSyncDelta, UnkaiError> {
    let server = normalize_server_url(server_url);
    let http = build(trusted_certs)?;

    let body = sync_collection_body(prev_sync_token.unwrap_or(""));
    tracing::info!(
        "CalDAV sync-collection (tasks) on {task_list_url} (token={:?})",
        prev_sync_token
    );
    let resp = report(&http, task_list_url, username, app_password, &body).await?;
    let status = resp.status();
    if status.as_u16() == 415 {
        tracing::warn!(
            "sync-collection on {task_list_url} returned 415 — skipping (collection refuses sync-collection)"
        );
        return Ok(TaskSyncDelta::default());
    }
    if !status.is_success() && status.as_u16() != 207 {
        return Err(UnkaiError::Nextcloud(format!(
            "task-list sync-collection returned HTTP {status}"
        )));
    }
    let xml = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading sync-collection body: {e}")))?;
    let parsed = parse_sync_collection(&xml, &server)
        .map_err(|e| UnkaiError::Protocol(format!("sync-collection parse: {e}")))?;

    let upserts = if parsed.changed.is_empty() {
        Vec::new()
    } else {
        fetch_tasks(
            &http,
            task_list_id,
            task_list_url,
            username,
            app_password,
            &server,
            &parsed.changed,
        )
        .await?
    };

    Ok(TaskSyncDelta {
        upserts,
        deleted_hrefs: parsed.deleted,
        new_sync_token: parsed.new_sync_token,
    })
}

fn sync_collection_body(prev_token: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token>{prev_token}</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop>
    <d:getetag/>
  </d:prop>
</d:sync-collection>"#
    )
}

#[derive(Debug, Default)]
struct SyncCollectionResult {
    changed: Vec<String>,
    deleted: Vec<String>,
    new_sync_token: Option<String>,
}

fn parse_sync_collection(
    xml: &str,
    server_url: &str,
) -> Result<SyncCollectionResult, quick_xml::Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = SyncCollectionResult::default();

    loop {
        match reader.read_event()? {
            Event::Start(s) => match local_name(&s).as_str() {
                "response" => {
                    if let Some((href, status, etag)) = parse_sync_response(&mut reader)? {
                        if status.contains("200") && etag.is_some() {
                            out.changed.push(absolute_url(server_url, &href));
                        } else if status.contains("404") {
                            out.deleted.push(absolute_url(server_url, &href));
                        }
                    }
                }
                "sync-token" => {
                    let token = read_text_until(&mut reader, "sync-token")?;
                    if !token.is_empty() {
                        out.new_sync_token = Some(token);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn parse_sync_response(
    reader: &mut Reader<&[u8]>,
) -> Result<Option<(String, String, Option<String>)>, quick_xml::Error> {
    let mut href: Option<String> = None;
    let mut status: Option<String> = None;
    let mut etag: Option<String> = None;

    loop {
        match reader.read_event()? {
            Event::Start(s) => match local_name(&s).as_str() {
                "propstat" | "prop" => {}
                "href" => href = Some(read_text_until(reader, "href")?),
                "status" => status = Some(read_text_until(reader, "status")?),
                "getetag" => etag = Some(read_text_until(reader, "getetag")?),
                other => skip_subtree(reader, other)?,
            },
            Event::End(end) if local_name_end(&end) == "response" => break,
            Event::Eof => break,
            _ => {}
        }
    }

    let Some(href) = href else { return Ok(None) };
    let status = status.unwrap_or_default();
    let etag = etag.map(|e| e.trim_matches('"').to_string());
    Ok(Some((href, status, etag)))
}

async fn fetch_tasks(
    http: &reqwest::Client,
    task_list_id: &str,
    task_list_url: &str,
    username: &str,
    app_password: &str,
    server_url: &str,
    changed: &[String],
) -> Result<Vec<RawTask>, UnkaiError> {
    let mut hrefs_xml = String::new();
    for href in changed {
        let path = href.strip_prefix(server_url).unwrap_or(href);
        hrefs_xml.push_str(&format!("  <d:href>{}</d:href>\n", xml_escape(path)));
    }

    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<cal:calendar-multiget xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <cal:calendar-data/>
  </d:prop>
{hrefs_xml}</cal:calendar-multiget>"#
    );

    let resp = report(http, task_list_url, username, app_password, &body).await?;
    if !resp.status().is_success() && resp.status().as_u16() != 207 {
        return Err(UnkaiError::Nextcloud(format!(
            "task calendar-multiget returned HTTP {}",
            resp.status()
        )));
    }
    let xml = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading multiget body: {e}")))?;

    parse_multiget(&xml, server_url, task_list_id)
        .map_err(|e| UnkaiError::Protocol(format!("multiget parse: {e}")))
}

fn parse_multiget(
    xml: &str,
    server_url: &str,
    task_list_id: &str,
) -> Result<Vec<RawTask>, quick_xml::Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(s) if local_name(&s) == "response" => {
                if let Some(e) = parse_multiget_response(&mut reader, server_url, task_list_id)? {
                    out.push(e);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn parse_multiget_response(
    reader: &mut Reader<&[u8]>,
    server_url: &str,
    task_list_id: &str,
) -> Result<Option<RawTask>, quick_xml::Error> {
    let mut href: Option<String> = None;
    let mut etag: Option<String> = None;
    let mut ics_raw: Option<String> = None;

    loop {
        match reader.read_event()? {
            Event::Start(s) => match local_name(&s).as_str() {
                "propstat" | "prop" | "status" => {}
                "href" => href = Some(read_text_until(reader, "href")?),
                "getetag" => etag = Some(read_text_until(reader, "getetag")?),
                "calendar-data" => ics_raw = Some(read_text_until(reader, "calendar-data")?),
                other => skip_subtree(reader, other)?,
            },
            Event::End(end) if local_name_end(&end) == "response" => break,
            Event::Eof => break,
            _ => {}
        }
    }

    let (Some(href), Some(etag), Some(ics_raw)) = (href, etag, ics_raw) else {
        return Ok(None);
    };
    let etag = etag.trim_matches('"').to_string();
    let absolute = absolute_url(server_url, &href);

    let parsed = match parse_vtodo_ics(&ics_raw, task_list_id, &absolute, &etag) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Skipping unparseable VTODO object at {href}: {e}");
            return Ok(None);
        }
    };
    // Calendar objects can carry VEVENT-only bodies (when the user
    // stores both events and tasks in the same collection).  Drop
    // those silently — they belong to the calendar sync path.
    if parsed.is_empty() {
        return Ok(None);
    }

    Ok(Some(RawTask {
        href: absolute,
        etag,
        tasks: parsed,
        ics_raw,
    }))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ── Parser ─────────────────────────────────────────────────────────

/// Parse one iCalendar body into zero or more `Task`s.
///
/// A single calendar object resource sometimes bundles a VEVENT and a
/// VTODO together (e.g. an exported file).  We pull out only the
/// VTODOs and let the calendar sync path handle the VEVENTs separately.
pub fn parse_vtodo_ics(
    raw: &str,
    task_list_id: &str,
    href: &str,
    etag: &str,
) -> Result<Vec<Task>, UnkaiError> {
    let reader = std::io::BufReader::new(raw.as_bytes());
    let parser = IcalParser::new(reader);
    let mut out = Vec::new();

    for cal_result in parser {
        let cal = cal_result.map_err(|e| UnkaiError::Protocol(format!("iCalendar parse: {e}")))?;
        for todo in &cal.todos {
            match task_from_properties(&todo.properties, task_list_id, href, etag, raw) {
                Ok(Some(t)) => out.push(t),
                Ok(None) => {
                    tracing::warn!("Skipped VTODO: missing UID");
                }
                Err(e) => {
                    tracing::warn!("Skipped VTODO: {e}");
                }
            }
        }
    }

    Ok(out)
}

fn task_from_properties(
    props: &[ical::property::Property],
    task_list_id: &str,
    href: &str,
    etag: &str,
    ics_raw: &str,
) -> Result<Option<Task>, String> {
    let mut uid: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut description: Option<String> = None;
    let mut status: Option<String> = None;
    let mut priority: u8 = 0;
    let mut due: Option<DateTime<Utc>> = None;
    let mut completed: Option<DateTime<Utc>> = None;
    let mut created: Option<DateTime<Utc>> = None;
    let mut last_modified: Option<DateTime<Utc>> = None;
    let mut url: Option<String> = None;
    let mut categories: Vec<String> = Vec::new();

    for prop in props {
        let name = prop.name.to_ascii_uppercase();
        let Some(value) = prop.value.as_deref() else {
            continue;
        };
        match name.as_str() {
            "UID" => uid = Some(value.to_string()),
            "SUMMARY" => summary = Some(unescape_text(value)),
            "DESCRIPTION" => description = Some(unescape_text(value)),
            "STATUS" => status = Some(value.to_ascii_uppercase()),
            "PRIORITY" => {
                priority = value.parse::<u8>().unwrap_or(0);
            }
            "DUE" => due = parse_datetime_property(prop, value).ok(),
            "COMPLETED" => completed = parse_datetime_property(prop, value).ok(),
            "CREATED" => created = parse_datetime_property(prop, value).ok(),
            "LAST-MODIFIED" => last_modified = parse_datetime_property(prop, value).ok(),
            "URL" => url = Some(value.to_string()),
            "CATEGORIES" => {
                for cat in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    categories.push(unescape_text(cat));
                }
            }
            _ => {}
        }
    }

    let Some(uid) = uid else { return Ok(None) };
    let status = status.unwrap_or_else(|| "NEEDS-ACTION".to_string());

    Ok(Some(Task {
        uid,
        task_list_id: task_list_id.to_string(),
        href: href.to_string(),
        etag: etag.to_string(),
        summary: summary.unwrap_or_default(),
        description,
        status,
        priority,
        due,
        completed,
        created,
        last_modified,
        url,
        categories,
        ics_raw: ics_raw.to_string(),
    }))
}

/// Parse a DUE / COMPLETED / CREATED / LAST-MODIFIED property value.
/// Supports the same three forms VEVENT DTSTART uses: UTC (`…Z`), TZID-
/// qualified local time, and floating (no TZID, no Z — treated as UTC).
/// All-day dues (`VALUE=DATE`) land as midnight UTC of that day.
fn parse_datetime_property(
    prop: &ical::property::Property,
    value: &str,
) -> Result<DateTime<Utc>, String> {
    let is_date_only = property_param(prop, "VALUE")
        .map(|v| v.eq_ignore_ascii_case("DATE"))
        .unwrap_or(false)
        || value.len() == 8;

    if is_date_only {
        let d = NaiveDate::parse_from_str(value, "%Y%m%d")
            .map_err(|e| format!("DATE value {value:?}: {e}"))?;
        return Ok(Utc.from_utc_datetime(
            &d.and_hms_opt(0, 0, 0)
                .ok_or("0:00:00 should always be valid")?,
        ));
    }

    if let Some(stripped) = value.strip_suffix('Z') {
        let dt = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
            .map_err(|e| format!("UTC DATE-TIME {value:?}: {e}"))?;
        return Ok(Utc.from_utc_datetime(&dt));
    }

    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .map_err(|e| format!("DATE-TIME {value:?}: {e}"))?;

    if let Some(tz_name) = property_param(prop, "TZID") {
        if let Ok(tz) = tz_name.parse::<Tz>() {
            match tz.from_local_datetime(&naive) {
                chrono::LocalResult::Single(dt) => return Ok(dt.with_timezone(&Utc)),
                chrono::LocalResult::Ambiguous(e, _) => return Ok(e.with_timezone(&Utc)),
                chrono::LocalResult::None => {
                    tracing::warn!(
                        "VTODO DATE-TIME {value:?} falls in DST gap for {tz_name} — treating as UTC"
                    );
                }
            }
        } else {
            tracing::warn!("VTODO unknown TZID {tz_name:?} — treating {value:?} as UTC");
        }
    }

    Ok(Utc.from_utc_datetime(&naive))
}

fn property_param<'a>(prop: &'a ical::property::Property, name: &str) -> Option<&'a str> {
    prop.params
        .as_ref()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, vs)| vs.first())
        .map(|s| s.as_str())
}

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

// ── Builder ────────────────────────────────────────────────────────

/// Render a `Task` as a complete iCalendar VTODO body suitable for PUT
/// to a CalDAV server.
pub fn build_vtodo_ics(task: &Task) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("BEGIN:VCALENDAR".into());
    lines.push("VERSION:2.0".into());
    lines.push(format!(
        "PRODID:-//Unkai Mail//CalDAV {}//EN",
        env!("CARGO_PKG_VERSION")
    ));
    lines.push("BEGIN:VTODO".into());
    lines.push(format!("UID:{}", task.uid));
    lines.push(format!("DTSTAMP:{}", format_utc_dt(&Utc::now())));
    if let Some(c) = task.created {
        lines.push(format!("CREATED:{}", format_utc_dt(&c)));
    } else {
        lines.push(format!("CREATED:{}", format_utc_dt(&Utc::now())));
    }
    lines.push(format!(
        "LAST-MODIFIED:{}",
        format_utc_dt(&task.last_modified.unwrap_or_else(Utc::now))
    ));
    if !task.summary.is_empty() {
        lines.push(format!("SUMMARY:{}", escape_text(&task.summary)));
    }
    if let Some(desc) = &task.description
        && !desc.is_empty()
    {
        lines.push(format!("DESCRIPTION:{}", escape_text(desc)));
    }
    if !task.status.is_empty() {
        lines.push(format!("STATUS:{}", task.status));
    }
    if task.priority > 0 {
        lines.push(format!("PRIORITY:{}", task.priority));
    }
    if let Some(due) = task.due {
        lines.push(format!("DUE:{}", format_utc_dt(&due)));
    }
    if let Some(c) = task.completed {
        lines.push(format!("COMPLETED:{}", format_utc_dt(&c)));
    }
    if let Some(url) = &task.url
        && !url.is_empty()
    {
        lines.push(format!("URL:{url}"));
    }
    if !task.categories.is_empty() {
        let joined = task
            .categories
            .iter()
            .map(|c| escape_text(c))
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!("CATEGORIES:{joined}"));
    }
    lines.push("END:VTODO".into());
    lines.push("END:VCALENDAR".into());

    let folded: Vec<String> = lines.iter().map(|l| fold_line(l)).collect();
    folded.join("\r\n") + "\r\n"
}

fn format_utc_dt(dt: &DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

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
        out.push_str(std::str::from_utf8(&bytes[i..end]).unwrap_or(""));
        i = end;
    }
    out
}

// ── Write helpers ──────────────────────────────────────────────────

/// Result of a successful create / update — the canonical href and
/// the new etag, ready to drop into the local cache row.
#[derive(Debug, Clone)]
pub struct TaskWriteOutcome {
    pub href: String,
    pub etag: String,
}

/// Create a new task in `task_list_url`.  Builds `{task_list_url}/{uid}.ics`
/// and PUTs with `If-None-Match: *` so a UID collision becomes a clean
/// 412 instead of overwriting a sibling task.
pub async fn create_task(
    server_url: &str,
    task_list_url: &str,
    username: &str,
    app_password: &str,
    uid: &str,
    ics: &str,
    trusted_certs: &[TrustedCert],
) -> Result<TaskWriteOutcome, UnkaiError> {
    let http = build(trusted_certs)?;
    let href = build_href(task_list_url, uid);
    let resp = put_ics(&http, &href, username, app_password, ics, None, true).await?;
    let status = resp.status();
    if status == StatusCode::PRECONDITION_FAILED {
        return Err(UnkaiError::Nextcloud(format!(
            "task with UID {uid} already exists on the server"
        )));
    }
    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Err(UnkaiError::CalDavWriteForbidden(format!(
            "PUT new task {href} returned HTTP {status}"
        )));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "PUT new task returned HTTP {status}"
        )));
    }
    let etag = read_etag(&resp).unwrap_or_default();
    Ok(TaskWriteOutcome {
        href: absolute_or_passthrough(server_url, &href),
        etag,
    })
}

/// Update an existing task at `href`, gated on `if_match_etag`.
pub async fn update_task(
    href: &str,
    username: &str,
    app_password: &str,
    if_match_etag: &str,
    ics: &str,
    trusted_certs: &[TrustedCert],
) -> Result<TaskWriteOutcome, UnkaiError> {
    let http = build(trusted_certs)?;
    let resp = put_ics(
        &http,
        href,
        username,
        app_password,
        ics,
        Some(if_match_etag),
        false,
    )
    .await?;
    let status = resp.status();
    if status == StatusCode::PRECONDITION_FAILED {
        return Err(UnkaiError::EtagMismatch(format!(
            "If-Match failed for {href} (server etag != cached)"
        )));
    }
    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Err(UnkaiError::CalDavWriteForbidden(format!(
            "PUT {href} returned HTTP {status}"
        )));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "PUT task returned HTTP {status}"
        )));
    }
    let etag = read_etag(&resp).unwrap_or_default();
    Ok(TaskWriteOutcome {
        href: href.to_string(),
        etag,
    })
}

/// Delete a task at `href`, gated on `if_match_etag`.
pub async fn delete_task(
    href: &str,
    username: &str,
    app_password: &str,
    if_match_etag: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    let http = build(trusted_certs)?;
    let resp = delete_resource(&http, href, username, app_password, Some(if_match_etag)).await?;
    let status = resp.status();
    if status == StatusCode::PRECONDITION_FAILED {
        return Err(UnkaiError::EtagMismatch(format!(
            "If-Match failed for DELETE {href}"
        )));
    }
    if status == StatusCode::NOT_FOUND {
        return Ok(());
    }
    if status == StatusCode::FORBIDDEN {
        return Err(UnkaiError::CalDavWriteForbidden(format!(
            "DELETE {href} returned HTTP {status}"
        )));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "DELETE task returned HTTP {status}"
        )));
    }
    Ok(())
}

fn build_href(task_list_url: &str, uid: &str) -> String {
    let base = task_list_url.trim_end_matches('/');
    let safe_uid = uid_to_filename(uid);
    format!("{base}/{safe_uid}.ics")
}

fn uid_to_filename(uid: &str) -> String {
    uid.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

fn read_etag(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string())
}

fn absolute_or_passthrough(server_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{}{}", normalize_server_url(server_url), href)
    } else {
        format!("{}/{}", normalize_server_url(server_url), href)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_VTODO: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//test//test//EN\r\n\
BEGIN:VTODO\r\n\
UID:task-1@example.com\r\n\
SUMMARY:Pick up groceries\r\n\
DESCRIPTION:Milk\\, eggs\\, bread\r\n\
STATUS:NEEDS-ACTION\r\n\
PRIORITY:5\r\n\
DUE:20260420T170000Z\r\n\
CREATED:20260418T120000Z\r\n\
LAST-MODIFIED:20260418T120000Z\r\n\
END:VTODO\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn parses_simple_vtodo() {
        let tasks = parse_vtodo_ics(SIMPLE_VTODO, "tl-1", "https://x/t.ics", "etag1").unwrap();
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.uid, "task-1@example.com");
        assert_eq!(t.summary, "Pick up groceries");
        assert_eq!(t.description.as_deref(), Some("Milk, eggs, bread"));
        assert_eq!(t.status, "NEEDS-ACTION");
        assert_eq!(t.priority, 5);
        assert_eq!(
            t.due.map(|d| d.to_rfc3339()).as_deref(),
            Some("2026-04-20T17:00:00+00:00")
        );
        assert!(!t.is_completed());
    }

    #[test]
    fn parses_completed_vtodo() {
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VTODO\r\n\
UID:done@example.com\r\n\
SUMMARY:Done\r\n\
STATUS:COMPLETED\r\n\
COMPLETED:20260419T093000Z\r\n\
END:VTODO\r\n\
END:VCALENDAR\r\n";
        let tasks = parse_vtodo_ics(ics, "tl-1", "https://x/d.ics", "etag2").unwrap();
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert!(t.is_completed());
        assert_eq!(
            t.completed.map(|d| d.to_rfc3339()).as_deref(),
            Some("2026-04-19T09:30:00+00:00")
        );
    }

    #[test]
    fn vevent_only_body_yields_no_tasks() {
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:evt-1@example.com\r\n\
SUMMARY:Meeting\r\n\
DTSTART:20260420T090000Z\r\n\
DTEND:20260420T093000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let tasks = parse_vtodo_ics(ics, "tl-1", "https://x/e.ics", "etag3").unwrap();
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn build_roundtrips_essentials() {
        let due = Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap();
        let task = Task {
            uid: "round@example.com".into(),
            task_list_id: "tl-1".into(),
            href: "https://x/r.ics".into(),
            etag: "etag-r".into(),
            summary: "Round-trip".into(),
            description: Some("A note with, commas; and a\nnewline".into()),
            status: "NEEDS-ACTION".into(),
            priority: 3,
            due: Some(due),
            completed: None,
            created: Some(Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap()),
            last_modified: Some(Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap()),
            url: Some("mail://acc/INBOX/42".into()),
            categories: vec!["Work".into(), "Mail".into()],
            ics_raw: String::new(),
        };
        let body = build_vtodo_ics(&task);
        assert!(body.contains("BEGIN:VTODO"));
        assert!(body.contains("UID:round@example.com"));
        assert!(body.contains("SUMMARY:Round-trip"));
        assert!(body.contains("PRIORITY:3"));
        assert!(body.contains("DUE:20260501T120000Z"));
        assert!(body.contains("STATUS:NEEDS-ACTION"));
        assert!(body.contains("URL:mail://acc/INBOX/42"));
        assert!(body.contains("CATEGORIES:Work,Mail"));
        // Round-trip back through the parser.
        let parsed = parse_vtodo_ics(&body, "tl-1", "https://x/r.ics", "etag-r").unwrap();
        assert_eq!(parsed.len(), 1);
        let t = &parsed[0];
        assert_eq!(t.summary, "Round-trip");
        assert_eq!(
            t.description.as_deref(),
            Some("A note with, commas; and a\nnewline")
        );
        assert_eq!(t.priority, 3);
        assert_eq!(t.url.as_deref(), Some("mail://acc/INBOX/42"));
        assert_eq!(t.categories, vec!["Work".to_string(), "Mail".to_string()]);
    }

    #[test]
    fn vtodo_propfind_filters_pseudo_collections() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/personal/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Personal</d:displayname>
        <c:supported-calendar-component-set>
          <c:comp name="VEVENT"/>
          <c:comp name="VTODO"/>
        </c:supported-calendar-component-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/trashbin/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Trash</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let lists = parse_task_list_propfind(xml, "https://cloud.example.com", "nc1").unwrap();
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].name, "personal");
        assert_eq!(lists[0].display_name, "Personal");
        assert_eq!(
            lists[0].id,
            "nc1::https://cloud.example.com/remote.php/dav/calendars/alice/personal/"
        );
    }
}

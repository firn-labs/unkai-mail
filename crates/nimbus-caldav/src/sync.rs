//! Incremental sync of one CalDAV calendar via RFC 6578
//! `sync-collection` REPORT, followed by `calendar-multiget` to fetch
//! the actual iCalendar bodies for the hrefs that changed.
//!
//! The protocol shape is identical to `nimbus-carddav::sync` â€” we pass
//! an opaque sync token, the server tells us what changed since, and
//! we fetch bodies in a second phase. The differences are cosmetic:
//!
//! - element names are `calendar-multiget` / `calendar-data` instead
//!   of `addressbook-multiget` / `address-data`
//! - XML namespace is `urn:ietf:params:xml:ns:caldav`
//! - the body format is `text/calendar`, not `text/vcard`
//!
//! # What we return
//!
//! A `CalendarSyncDelta` with:
//!   - `upserts` â€” one `RawEvent` per changed calendar object. Each
//!     may contain zero or more VEVENTs (one object = one file, but a
//!     recurring series can bundle master + overrides).
//!   - `deleted_hrefs` â€” server paths the store should remove.
//!   - `new_sync_token` â€” opaque, pass back on the next call.
//!
//! Caller responsibility (the Tauri command):
//!   1. Persist `new_sync_token` alongside the calendar row.
//!   2. For each `RawEvent`, loop over `events` and upsert by
//!      `(calendar_id, event.id /* VEVENT UID */)`.
//!   3. Delete cached events whose href matches any entry in
//!      `deleted_hrefs`.

use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};

use nimbus_core::NimbusError;
use nimbus_core::models::CalendarEvent;
use nimbus_core::models::TrustedCert;

use crate::client::{absolute_url, build, normalize_server_url, report};
use crate::ical::parse_ics;
use crate::xml_util::{local_name, local_name_end, read_text_until, skip_subtree};

/// One calendar object resource on the server, plus the zero-or-more
/// VEVENTs we parsed out of its iCalendar body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEvent {
    /// Absolute URL of the resource (matches `href` in the sync-collection
    /// response, suitable for later PUT/DELETE via `If-Match`).
    pub href: String,
    /// Server etag for the resource â€” the key concurrency primitive
    /// for a future write path.
    pub etag: String,
    /// The parsed VEVENT(s) in this object. Usually one, but
    /// recurring series can bundle the master + RECURRENCE-ID
    /// overrides into a single file.
    pub events: Vec<CalendarEvent>,
    /// Raw iCalendar text â€” kept so the store can re-parse later
    /// without re-syncing, same as `vcard_raw` in carddav.
    pub ics_raw: String,
}

/// Result of one sync round.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalendarSyncDelta {
    pub upserts: Vec<RawEvent>,
    pub deleted_hrefs: Vec<String>,
    pub new_sync_token: Option<String>,
}

/// Sync a single calendar.
///
/// Pass `prev_sync_token = None` (or an empty string) for the initial
/// pull. The returned `new_sync_token` should be persisted so the
/// next call is incremental.
pub async fn sync_calendar(
    server_url: &str,
    calendar_url: &str,
    username: &str,
    app_password: &str,
    prev_sync_token: Option<&str>,
    trusted_certs: &[TrustedCert],
) -> Result<CalendarSyncDelta, NimbusError> {
    let server = normalize_server_url(server_url);
    let http = build(trusted_certs)?;

    // Phase 1: sync-collection.
    let body = sync_collection_body(prev_sync_token.unwrap_or(""));
    tracing::info!(
        "CalDAV sync-collection on {calendar_url} (token={:?})",
        prev_sync_token
    );
    let resp = report(&http, calendar_url, username, app_password, &body).await?;
    let status = resp.status();
    // Some servers refuse sync-collection on certain collections â€” skip
    // quietly instead of failing the whole sync (same belt-and-braces
    // behaviour as carddav).
    if status.as_u16() == 415 {
        tracing::warn!(
            "sync-collection on {calendar_url} returned 415 â€” skipping (likely a \
             pseudo-calendar that doesn't support sync-collection)"
        );
        return Ok(CalendarSyncDelta::default());
    }
    if !status.is_success() && status.as_u16() != 207 {
        return Err(NimbusError::Nextcloud(format!(
            "calendar sync-collection returned HTTP {status}"
        )));
    }
    let xml = resp
        .text()
        .await
        .map_err(|e| NimbusError::Network(format!("reading sync-collection body: {e}")))?;
    let parsed = parse_sync_collection(&xml, &server)
        .map_err(|e| NimbusError::Protocol(format!("sync-collection parse: {e}")))?;

    // Phase 2: calendar-multiget for the changed hrefs.
    let upserts = if parsed.changed.is_empty() {
        Vec::new()
    } else {
        fetch_events(
            &http,
            calendar_url,
            username,
            app_password,
            &server,
            &parsed.changed,
        )
        .await?
    };

    Ok(CalendarSyncDelta {
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
    changed: Vec<ChangedHref>,
    deleted: Vec<String>,
    new_sync_token: Option<String>,
}

#[derive(Debug, Clone)]
struct ChangedHref {
    href: String,
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
                            out.changed.push(ChangedHref {
                                href: absolute_url(server_url, &href),
                            });
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

/// Phase 2: fetch the iCalendar bodies for the changed hrefs.
async fn fetch_events(
    http: &reqwest::Client,
    calendar_url: &str,
    username: &str,
    app_password: &str,
    server_url: &str,
    changed: &[ChangedHref],
) -> Result<Vec<RawEvent>, NimbusError> {
    let mut hrefs_xml = String::new();
    for c in changed {
        // Convert back to a server-relative path â€” multiget wants the
        // same form the server originally returned.
        let path = c.href.strip_prefix(server_url).unwrap_or(&c.href);
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

    let resp = report(http, calendar_url, username, app_password, &body).await?;
    if !resp.status().is_success() && resp.status().as_u16() != 207 {
        return Err(NimbusError::Nextcloud(format!(
            "calendar-multiget returned HTTP {}",
            resp.status()
        )));
    }
    let xml = resp
        .text()
        .await
        .map_err(|e| NimbusError::Network(format!("reading multiget body: {e}")))?;

    parse_multiget(&xml, server_url)
        .map_err(|e| NimbusError::Protocol(format!("multiget parse: {e}")))
}

fn parse_multiget(xml: &str, server_url: &str) -> Result<Vec<RawEvent>, quick_xml::Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(s) if local_name(&s) == "response" => {
                if let Some(e) = parse_multiget_response(&mut reader, server_url)? {
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
) -> Result<Option<RawEvent>, quick_xml::Error> {
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

    // Skip objects that fail to parse rather than failing the entire
    // sync â€” one malformed VEVENT shouldn't break everything else.
    let events = match parse_ics(&ics_raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Skipping unparseable calendar object at {href}: {e}");
            return Ok(None);
        }
    };

    Ok(Some(RawEvent {
        href: absolute_url(server_url, &href),
        etag,
        events,
        ics_raw,
    }))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sync_collection_with_change_and_delete() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/cal/e1.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"abc"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/cal/e2.ics</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:sync-token>http://nc/ns/sync/55</d:sync-token>
</d:multistatus>"#;
        let r = parse_sync_collection(xml, "https://cloud.example.com").unwrap();
        assert_eq!(r.changed.len(), 1);
        assert_eq!(
            r.changed[0].href,
            "https://cloud.example.com/dav/cal/e1.ics"
        );
        assert_eq!(r.deleted, vec!["https://cloud.example.com/dav/cal/e2.ics"]);
        assert_eq!(r.new_sync_token.as_deref(), Some("http://nc/ns/sync/55"));
    }

    #[test]
    fn parses_multiget_with_inline_calendar_data() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/cal/e1.ics</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>"v1"</d:getetag>
        <cal:calendar-data><![CDATA[BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:e1@example.com
SUMMARY:Hello
DTSTART:20260420T090000Z
DTEND:20260420T093000Z
END:VEVENT
END:VCALENDAR
]]></cal:calendar-data>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let out = parse_multiget(xml, "https://cloud.example.com").unwrap();
        assert_eq!(out.len(), 1);
        let e = &out[0];
        assert_eq!(e.href, "https://cloud.example.com/dav/cal/e1.ics");
        assert_eq!(e.etag, "v1");
        assert_eq!(e.events.len(), 1);
        assert_eq!(e.events[0].id, "e1@example.com");
        assert_eq!(e.events[0].summary, "Hello");
    }
}

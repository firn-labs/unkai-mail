//! List the calendars owned by a Nextcloud user.
//!
//! Nextcloud puts every user's calendars under a stable home URL:
//!
//! ```text
//! /remote.php/dav/calendars/<username>/
//! ```
//!
//! A PROPFIND with `Depth: 1` returns the home plus one `<response>`
//! per child collection. We keep only the ones whose `<resourcetype>`
//! contains a CalDAV `<calendar/>` marker â€” Nextcloud also exposes
//! pseudo-collections (trash, birthday feeds, subscriptions) at the
//! same depth, and some of those refuse `sync-collection` REPORTs,
//! so filtering them here prevents broken syncs later.
//!
//! # calendar-color
//!
//! Nextcloud advertises a per-calendar hex colour via the
//! `<apple:calendar-color>` extension (`xmlns:apple="http://apple.com/ns/ical/"`).
//! We capture it when present â€” the UI can use it for chips and event
//! dots. Missing is fine; not every server implements it.

use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};

use nimbus_core::NimbusError;
use nimbus_core::models::TrustedCert;

use crate::client::{absolute_url, build, normalize_server_url, propfind};
use crate::xml_util::{local_name, local_name_end, read_text_until, skip_subtree};

/// One calendar on the server.
///
/// `path` is the absolute URL used for sync REPORTs (already resolved).
/// `name` is the slug at the end of `path` â€” stable identifier for the
/// local cache even if `display_name` changes server-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calendar {
    pub path: String,
    pub name: String,
    pub display_name: Option<String>,
    pub color: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
}

/// PROPFIND body. Only requests the props we consume.
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/" xmlns:apple="http://apple.com/ns/ical/">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <d:sync-token/>
    <cs:getctag/>
    <apple:calendar-color/>
  </d:prop>
</d:propfind>"#;

/// List all calendars owned by `username` on `server_url`.
pub async fn list_calendars(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<Calendar>, NimbusError> {
    let server = normalize_server_url(server_url);
    let home = format!("{server}/remote.php/dav/calendars/{username}/");
    tracing::info!("CalDAV PROPFIND home: {home}");

    let http = build(trusted_certs)?;
    let resp = propfind(&http, &home, username, app_password, 1, PROPFIND_BODY).await?;

    if !resp.status().is_success() && resp.status().as_u16() != 207 {
        return Err(NimbusError::Nextcloud(format!(
            "calendar PROPFIND returned HTTP {}",
            resp.status()
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| NimbusError::Network(format!("reading PROPFIND body: {e}")))?;

    parse_calendar_list(&body, &server)
}

fn parse_calendar_list(xml: &str, server_url: &str) -> Result<Vec<Calendar>, NimbusError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut cals = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(s)) if local_name(&s) == "response" => {
                if let Some(cal) = parse_response(&mut reader, server_url)
                    .map_err(|e| NimbusError::Protocol(format!("CalDAV XML: {e}")))?
                {
                    cals.push(cal);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(NimbusError::Protocol(format!("CalDAV XML: {e}"))),
            _ => {}
        }
    }

    // Drop pseudo-calendars Nextcloud exposes at the same depth:
    //   - `inbox` / `outbox` â€” CalDAV scheduling endpoints, not event stores
    //   - `trashbin` â€” Nextcloud's server-side trash (415s on sync-collection)
    //   - `z-app-generated--â€¦` â€” birthday feeds etc.
    cals.retain(|c| !is_pseudo_calendar(&c.name));

    tracing::info!("CalDAV: discovered {} calendar(s)", cals.len());
    Ok(cals)
}

fn is_pseudo_calendar(name: &str) -> bool {
    matches!(name, "inbox" | "outbox" | "trashbin") || name.starts_with("z-app-generated")
}

/// Walk a single `<response>` and pull out the bits we need.
/// Returns `Ok(None)` if it isn't a calendar (e.g. the home collection).
fn parse_response(
    reader: &mut Reader<&[u8]>,
    server_url: &str,
) -> Result<Option<Calendar>, quick_xml::Error> {
    let mut href: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut color: Option<String> = None;
    let mut ctag: Option<String> = None;
    let mut sync_token: Option<String> = None;
    let mut is_calendar = false;

    loop {
        match reader.read_event()? {
            Event::Start(s) => match local_name(&s).as_str() {
                "propstat" | "prop" | "status" => {}
                "href" => href = Some(read_text_until(reader, "href")?),
                "resourcetype" => {
                    // Walk the resourcetype subtree looking for a
                    // <calendar/> child. quick-xml surfaces both the
                    // `<calendar/>` self-closing form (Event::Empty)
                    // and the paired-open-close form (Event::Start) â€”
                    // match either.
                    loop {
                        match reader.read_event()? {
                            Event::Empty(e) | Event::Start(e) if local_name(&e) == "calendar" => {
                                is_calendar = true;
                            }
                            Event::End(e) if local_name_end(&e) == "resourcetype" => break,
                            Event::Eof => break,
                            _ => {}
                        }
                    }
                }
                "displayname" => display_name = Some(read_text_until(reader, "displayname")?),
                "calendar-color" => color = Some(read_text_until(reader, "calendar-color")?),
                "getctag" => ctag = Some(read_text_until(reader, "getctag")?),
                "sync-token" => sync_token = Some(read_text_until(reader, "sync-token")?),
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

    let trimmed = href.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed).to_string();

    Ok(Some(Calendar {
        path: absolute_url(server_url, &href),
        name,
        display_name: display_name.filter(|s| !s.is_empty()),
        color: color.filter(|s| !s.is_empty()),
        ctag: ctag.filter(|s| !s.is_empty()),
        sync_token: sync_token.filter(|s| !s.is_empty()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:apple="http://apple.com/ns/ical/">
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:displayname>alice</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/personal/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype>
          <d:collection/>
          <cal:calendar/>
        </d:resourcetype>
        <d:displayname>Personal</d:displayname>
        <apple:calendar-color>#1d63ed</apple:calendar-color>
        <cs:getctag>etag-007</cs:getctag>
        <d:sync-token>http://nc/ns/sync/17</d:sync-token>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/trashbin/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
        <d:displayname>Trash</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn parses_single_calendar_and_skips_home_and_trashbin() {
        let cals = parse_calendar_list(SAMPLE, "https://cloud.example.com").unwrap();
        assert_eq!(cals.len(), 1);
        let c = &cals[0];
        assert_eq!(c.name, "personal");
        assert_eq!(c.display_name.as_deref(), Some("Personal"));
        assert_eq!(c.color.as_deref(), Some("#1d63ed"));
        assert_eq!(c.ctag.as_deref(), Some("etag-007"));
        assert_eq!(c.sync_token.as_deref(), Some("http://nc/ns/sync/17"));
        assert_eq!(
            c.path,
            "https://cloud.example.com/remote.php/dav/calendars/alice/personal/"
        );
    }

    #[test]
    fn filters_app_generated_pseudo_calendars() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/personal/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
        <d:displayname>Personal</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/z-app-generated--contacts--birthdays/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
        <d:displayname>Birthdays</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let cals = parse_calendar_list(xml, "https://cloud.example.com").unwrap();
        assert_eq!(cals.len(), 1);
        assert_eq!(cals[0].name, "personal");
    }
}

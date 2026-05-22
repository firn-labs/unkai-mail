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

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;

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
    /// True when the user can only *read* this calendar — typical
    /// for shared calendars where the owner granted view-only
    /// access.  Derived from the CalDAV
    /// `current-user-privilege-set` PROPFIND prop (RFC 3744 §5.4):
    /// no `write` / `write-content` / `write-properties` privilege
    /// in the set means the user can't add events or change
    /// existing ones.  Servers that don't advertise the prop
    /// default to writable so the existing happy path is
    /// preserved (#236).
    #[serde(default)]
    pub read_only: bool,
}

/// PROPFIND body. Only requests the props we consume.
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/" xmlns:apple="http://apple.com/ns/ical/">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <d:sync-token/>
    <d:current-user-privilege-set/>
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
) -> Result<Vec<Calendar>, UnkaiError> {
    let server = normalize_server_url(server_url);
    let home = format!("{server}/remote.php/dav/calendars/{username}/");
    tracing::info!("CalDAV PROPFIND home: {home}");

    let http = build(trusted_certs)?;
    let resp = propfind(&http, &home, username, app_password, 1, PROPFIND_BODY).await?;

    if !resp.status().is_success() && resp.status().as_u16() != 207 {
        return Err(UnkaiError::Nextcloud(format!(
            "calendar PROPFIND returned HTTP {}",
            resp.status()
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading PROPFIND body: {e}")))?;

    parse_calendar_list(&body, &server)
}

fn parse_calendar_list(xml: &str, server_url: &str) -> Result<Vec<Calendar>, UnkaiError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut cals = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(s)) if local_name(&s) == "response" => {
                if let Some(cal) = parse_response(&mut reader, server_url)
                    .map_err(|e| UnkaiError::Protocol(format!("CalDAV XML: {e}")))?
                {
                    cals.push(cal);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(UnkaiError::Protocol(format!("CalDAV XML: {e}"))),
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
    // `None` until a privilege set arrives.  Servers that don't
    // advertise the prop leave this `None`, which `unwrap_or(false)`
    // below treats as "writable" — preserves the existing happy path
    // for servers that don't speak privilege-set.
    let mut read_only: Option<bool> = None;

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
                "current-user-privilege-set" => {
                    // RFC 3744 §5.4 — list of <privilege> entries the
                    // current user has on this resource.  We treat
                    // the calendar as writable if any of `write`,
                    // `write-content`, or `write-properties` is
                    // present; absent means read-only.  We don't
                    // parse the per-privilege content since CalDAV
                    // only nests one privilege element per
                    // `<privilege>` and the local name is enough.
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
        read_only: read_only.unwrap_or(false),
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
    fn parses_read_only_privilege_set() {
        // Two shared calendars: one with write privileges (full
        // shared edit access — not read-only) and one with only
        // read privileges (#236).  A third calendar omits the
        // privilege-set entirely so we lock in the
        // "default to writable" fallback for servers that don't
        // advertise the prop.
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/shared-rw/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
        <d:displayname>Shared (read-write)</d:displayname>
        <d:current-user-privilege-set>
          <d:privilege><d:read/></d:privilege>
          <d:privilege><d:write/></d:privilege>
          <d:privilege><d:write-content/></d:privilege>
        </d:current-user-privilege-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/shared-ro/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
        <d:displayname>Shared (read-only)</d:displayname>
        <d:current-user-privilege-set>
          <d:privilege><d:read/></d:privilege>
        </d:current-user-privilege-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/alice/no-priv-set/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
        <d:displayname>Server without priv-set</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let cals = parse_calendar_list(xml, "https://cloud.example.com").unwrap();
        assert_eq!(cals.len(), 3);
        let by_name: std::collections::HashMap<&str, &Calendar> =
            cals.iter().map(|c| (c.name.as_str(), c)).collect();
        assert_eq!(by_name["shared-rw"].read_only, false);
        assert_eq!(by_name["shared-ro"].read_only, true);
        // No privilege-set advertised → preserve pre-#236
        // happy path: assume writable.
        assert_eq!(by_name["no-priv-set"].read_only, false);
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

//! List the addressbooks owned by a Nextcloud user.
//!
//! Nextcloud puts every user's addressbooks under a stable home URL:
//!
//! ```text
//! /remote.php/dav/addressbooks/users/<username>/
//! ```
//!
//! A PROPFIND with Depth: 1 returns the home plus one `<response>` per
//! child collection. We filter to those whose `<resourcetype>` contains
//! a CardDAV `<addressbook/>` marker â€” Nextcloud also exposes a
//! "system" pseudo-collection at the same depth that we want to skip.

use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};

use nimbus_core::NimbusError;
use nimbus_core::models::TrustedCert;

use crate::client::{absolute_url, build, normalize_server_url, propfind};
use crate::xml_util::{local_name, read_text_until, skip_subtree};

/// One addressbook on the server.
///
/// `path` is the absolute URL we'll use for sync REPORTs (already
/// resolved against the server base). `name` is the slug at the end
/// of `path` â€” useful as a stable identifier in the local cache,
/// since `display_name` can change on the server side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Addressbook {
    pub path: String,
    pub name: String,
    pub display_name: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
}

/// PROPFIND body. Only requests the props we actually consume â€”
/// avoids dragging back the full deep tree some servers return when
/// you ask for `<allprop/>`.
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <d:sync-token/>
    <cs:getctag/>
  </d:prop>
</d:propfind>"#;

/// List all addressbooks owned by `username` on `server_url`.
///
/// Returns `Ok(vec)` even if the user has zero addressbooks â€” that's
/// a valid state on a fresh Nextcloud install. Network / auth /
/// parse failures all surface as `Err`.
pub async fn list_addressbooks(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<Addressbook>, NimbusError> {
    let server = normalize_server_url(server_url);
    let home = format!("{server}/remote.php/dav/addressbooks/users/{username}/");
    tracing::info!("CardDAV PROPFIND home: {home}");

    let http = build(trusted_certs)?;
    let resp = propfind(&http, &home, username, app_password, 1, PROPFIND_BODY).await?;

    if !resp.status().is_success() && resp.status().as_u16() != 207 {
        return Err(NimbusError::Nextcloud(format!(
            "addressbook PROPFIND returned HTTP {}",
            resp.status()
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| NimbusError::Network(format!("reading PROPFIND body: {e}")))?;

    parse_addressbook_list(&body, &server)
}

/// Pull `<response>` elements out of a multistatus body and turn the
/// addressbook ones into `Addressbook` records.
fn parse_addressbook_list(xml: &str, server_url: &str) -> Result<Vec<Addressbook>, NimbusError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut books = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(s)) if local_name(&s) == "response" => {
                if let Some(book) = parse_response(&mut reader, server_url)
                    .map_err(|e| NimbusError::Protocol(format!("CardDAV XML: {e}")))?
                {
                    books.push(book);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(NimbusError::Protocol(format!("CardDAV XML: {e}"))),
            _ => {}
        }
    }

    // Drop Nextcloud's app-generated pseudo-addressbooks. These show up
    // in the home collection alongside real ones but aren't designed to
    // be synced by external clients â€” `contactsinteraction--recent` in
    // particular returns HTTP 415 to sync-collection REPORTs. The
    // official NC clients filter them the same way.
    books.retain(|b| !is_pseudo_addressbook(&b.name));

    tracing::info!("CardDAV: discovered {} addressbook(s)", books.len());
    Ok(books)
}

/// True for Nextcloud system / app-generated addressbooks that look
/// like normal collections but aren't meant for client sync.
fn is_pseudo_addressbook(name: &str) -> bool {
    name.starts_with("z-app-generated") || name == "system"
}

/// Walk a single `<response>` and pull out the bits we need.
/// Returns `Ok(None)` if the response is not for an addressbook (e.g.
/// the home collection itself, or some other resource type).
fn parse_response(
    reader: &mut Reader<&[u8]>,
    server_url: &str,
) -> Result<Option<Addressbook>, quick_xml::Error> {
    let mut href: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut ctag: Option<String> = None;
    let mut sync_token: Option<String> = None;
    let mut is_addressbook = false;

    loop {
        match reader.read_event()? {
            Event::Start(s) => match local_name(&s).as_str() {
                // Transparent wrappers â€” descend without taking action.
                "propstat" | "prop" | "status" => {}
                "href" => href = Some(read_text_until(reader, "href")?),
                "resourcetype" => {
                    // Walk the resourcetype subtree looking for an
                    // <addressbook/> child (any namespace prefix).
                    // We only need to flip a flag â€” anything else
                    // inside resourcetype is fine to ignore; the loop
                    // exits cleanly at </resourcetype>.
                    loop {
                        match reader.read_event()? {
                            Event::Empty(e) | Event::Start(e)
                                if local_name(&e) == "addressbook" =>
                            {
                                is_addressbook = true;
                            }
                            Event::End(e) if local_name_end(&e) == "resourcetype" => break,
                            Event::Eof => break,
                            _ => {}
                        }
                    }
                }
                "displayname" => display_name = Some(read_text_until(reader, "displayname")?),
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
    if !is_addressbook {
        return Ok(None);
    }

    // Trim non-empty href; derive the addressbook slug from the last
    // non-empty path segment.
    let trimmed = href.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed).to_string();
    let display_name = display_name.filter(|s| !s.is_empty());

    Ok(Some(Addressbook {
        path: absolute_url(server_url, &href),
        name,
        display_name,
        ctag: ctag.filter(|s| !s.is_empty()),
        sync_token: sync_token.filter(|s| !s.is_empty()),
    }))
}

fn local_name_end(end: &quick_xml::events::BytesEnd<'_>) -> String {
    let bytes_owned = end.name();
    let bytes = bytes_owned.as_ref();
    let local = match bytes.iter().position(|&b| b == b':') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    };
    String::from_utf8_lossy(local).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sample multistatus shape Nextcloud returns. Trimmed to the
    /// minimum that exercises the parser's filter logic.
    const SAMPLE: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/remote.php/dav/addressbooks/users/alice/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:displayname>alice</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/addressbooks/users/alice/contacts/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype>
          <d:collection/>
          <card:addressbook/>
        </d:resourcetype>
        <d:displayname>Contacts</d:displayname>
        <cs:getctag>etag-001</cs:getctag>
        <d:sync-token>http://nc/ns/sync/42</d:sync-token>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn filters_app_generated_pseudo_addressbooks() {
        // Same shape as SAMPLE plus a `z-app-generated--â€¦--recent` book.
        // Real on Nextcloud; rejects sync-collection with HTTP 415.
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/remote.php/dav/addressbooks/users/alice/contacts/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><card:addressbook/></d:resourcetype>
        <d:displayname>Contacts</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/addressbooks/users/alice/z-app-generated--contactsinteraction--recent/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><card:addressbook/></d:resourcetype>
        <d:displayname>Recently contacted</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let books = parse_addressbook_list(xml, "https://cloud.example.com").unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].name, "contacts");
    }

    #[test]
    fn parses_single_addressbook_and_skips_home() {
        let books = parse_addressbook_list(SAMPLE, "https://cloud.example.com").unwrap();
        assert_eq!(books.len(), 1);
        let b = &books[0];
        assert_eq!(b.name, "contacts");
        assert_eq!(b.display_name.as_deref(), Some("Contacts"));
        assert_eq!(b.ctag.as_deref(), Some("etag-001"));
        assert_eq!(b.sync_token.as_deref(), Some("http://nc/ns/sync/42"));
        assert_eq!(
            b.path,
            "https://cloud.example.com/remote.php/dav/addressbooks/users/alice/contacts/"
        );
    }
}

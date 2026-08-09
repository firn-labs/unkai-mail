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
//! a CardDAV `<addressbook/>` marker — Nextcloud also exposes a
//! "system" pseudo-collection at the same depth that we want to skip.

use quick_xml::Reader;
use quick_xml::events::Event;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;

use crate::client::{absolute_url, build, build_no_redirect, normalize_server_url, propfind};
use crate::xml_util::{local_name, read_scalar_until, skip_subtree};

/// One addressbook on the server.
///
/// `path` is the absolute URL we'll use for sync REPORTs (already
/// resolved against the server base). `name` is the slug at the end
/// of `path` — useful as a stable identifier in the local cache,
/// since `display_name` can change on the server side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Addressbook {
    pub path: String,
    pub name: String,
    pub display_name: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
}

/// PROPFIND body. Only requests the props we actually consume —
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
/// Returns `Ok(vec)` even if the user has zero addressbooks — that's
/// a valid state on a fresh Nextcloud install. Network / auth /
/// parse failures all surface as `Err`.
pub async fn list_addressbooks(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<Addressbook>, UnkaiError> {
    let server = normalize_server_url(server_url);
    let home = format!("{server}/remote.php/dav/addressbooks/users/{username}/");
    list_addressbooks_at(&home, username, app_password, trusted_certs).await
}

/// List the addressbooks under an explicit collection-home URL (#413).
///
/// The Nextcloud path above derives the home from the server layout;
/// generic CardDAV servers store their (RFC 6764-resolved) home on
/// the account record and pass it here. Hrefs in the response are
/// resolved against the home's origin, since a generic server's base
/// URL may itself carry a path (e.g. `https://host/dav.php`).
pub async fn list_addressbooks_at(
    home_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<Addressbook>, UnkaiError> {
    tracing::info!("CardDAV PROPFIND home: {home_url}");

    let http = build(trusted_certs)?;
    let resp = propfind(&http, home_url, username, app_password, 1, PROPFIND_BODY).await?;

    if !resp.status().is_success() && resp.status().as_u16() != 207 {
        return Err(UnkaiError::Nextcloud(format!(
            "addressbook PROPFIND returned HTTP {}",
            resp.status()
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading PROPFIND body: {e}")))?;

    parse_addressbook_list(&body, &origin_of(home_url)?)
}

// ── RFC 6764 home resolution (#413) ─────────────────────────────
//
// Generic CardDAV servers don't share Nextcloud's fixed layout, so
// the addressbook home has to be discovered the standard way:
//
//   1. PROPFIND `current-user-principal` on `/.well-known/carddav`
//      (following redirects manually — the well-known endpoint
//      normally 301s to the real DAV context path, and reqwest's
//      auto-redirect would downgrade the PROPFIND to a GET).
//   2. PROPFIND `addressbook-home-set` on the principal URL.
//
// If the well-known route fails we retry step 1 against the URL the
// user typed (covers servers that answer at their root, and users
// who pasted the DAV context path directly), and as a last resort we
// accept the typed URL as the home itself when it answers a
// multistatus PROPFIND — that's what "I pasted my addressbook home"
// looks like.

const PRINCIPAL_PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-principal/>
  </d:prop>
</d:propfind>"#;

const HOME_SET_PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <card:addressbook-home-set/>
  </d:prop>
</d:propfind>"#;

/// Resolve the absolute addressbook-home URL for `username` on a
/// generic CardDAV server. `server_url` is whatever the user typed —
/// an origin, a DAV context path, or the home itself.
pub async fn resolve_addressbook_home(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<String, UnkaiError> {
    let server = normalize_server_url(server_url);
    let http = build_no_redirect(trusted_certs)?;
    let origin = origin_of(&server)?;

    // Principal-discovery starting points, in order.
    let candidates = [format!("{origin}/.well-known/carddav"), server.clone()];
    for candidate in &candidates {
        if let Some(home) = try_resolve_home(&http, candidate, username, app_password).await? {
            tracing::info!("CardDAV home resolved via {candidate}: {home}");
            return Ok(home);
        }
    }

    // Last resort: the typed URL may itself be the home collection.
    let resp = propfind(&http, &server, username, app_password, 0, PROPFIND_BODY).await?;
    if resp.status().is_success() || resp.status().as_u16() == 207 {
        tracing::info!("CardDAV: treating '{server}' as the addressbook home directly");
        return Ok(server);
    }

    Err(UnkaiError::Nextcloud(format!(
        "could not locate a CardDAV addressbook home on '{server}' (HTTP {})",
        resp.status()
    )))
}

/// One rung of the resolution ladder: principal discovery starting at
/// `start_url`, then the home-set lookup on the principal. `Ok(None)`
/// means "this route didn't pan out, try the next one"; only auth
/// failures and transport errors abort the whole ladder.
async fn try_resolve_home(
    http: &Client,
    start_url: &str,
    username: &str,
    app_password: &str,
) -> Result<Option<String>, UnkaiError> {
    let (final_url, resp) = propfind_following_redirects(
        http,
        start_url,
        username,
        app_password,
        0,
        PRINCIPAL_PROPFIND_BODY,
    )
    .await?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Nextcloud(
            "CardDAV authentication failed (HTTP 401) — check the username and password".into(),
        ));
    }
    if !status.is_success() && status.as_u16() != 207 {
        return Ok(None);
    }
    let xml = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading principal PROPFIND body: {e}")))?;
    let Some(principal_href) = extract_nested_href(&xml, "current-user-principal") else {
        return Ok(None);
    };
    let principal_url = absolute_url(&origin_of(&final_url)?, &principal_href);

    let (home_final, home_resp) = propfind_following_redirects(
        http,
        &principal_url,
        username,
        app_password,
        0,
        HOME_SET_PROPFIND_BODY,
    )
    .await?;
    let status = home_resp.status();
    if !status.is_success() && status.as_u16() != 207 {
        return Ok(None);
    }
    let xml = home_resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading home-set PROPFIND body: {e}")))?;
    Ok(extract_nested_href(&xml, "addressbook-home-set")
        .map(|href| absolute_url(&origin_of(&home_final).unwrap_or_default(), &href)))
}

/// PROPFIND that follows redirects by hand, repeating the same
/// method at each hop (see the module comment — auto-redirect would
/// turn the PROPFIND into a GET). Returns the final URL alongside
/// the response so hrefs can be resolved against the right origin.
async fn propfind_following_redirects(
    http: &Client,
    url: &str,
    username: &str,
    app_password: &str,
    depth: u32,
    body: &str,
) -> Result<(String, reqwest::Response), UnkaiError> {
    let mut current = url.to_string();
    for _ in 0..5 {
        let resp = propfind(http, &current, username, app_password, depth, body).await?;
        if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    UnkaiError::Nextcloud(format!("redirect from '{current}' without a Location"))
                })?;
            let base = reqwest::Url::parse(&current)
                .map_err(|e| UnkaiError::Other(format!("invalid URL '{current}': {e}")))?;
            current = base
                .join(loc)
                .map_err(|e| UnkaiError::Other(format!("invalid redirect target '{loc}': {e}")))?
                .to_string();
            continue;
        }
        return Ok((current, resp));
    }
    Err(UnkaiError::Nextcloud(format!(
        "too many redirects while resolving '{url}'"
    )))
}

/// `scheme://host[:port]` of a URL — the base hrefs get resolved
/// against, since DAV hrefs are server-absolute paths.
fn origin_of(url: &str) -> Result<String, UnkaiError> {
    let u = reqwest::Url::parse(url)
        .map_err(|e| UnkaiError::Other(format!("invalid URL '{url}': {e}")))?;
    let mut origin = format!(
        "{}://{}",
        u.scheme(),
        u.host_str()
            .ok_or_else(|| UnkaiError::Other(format!("URL '{url}' has no host")))?
    );
    if let Some(port) = u.port() {
        origin.push_str(&format!(":{port}"));
    }
    Ok(origin)
}

/// Pull the first `<href>` nested inside the first `<{prop}>` element
/// out of a multistatus body — enough for `current-user-principal`
/// and `addressbook-home-set`, which both wrap a single href.
fn extract_nested_href(xml: &str, prop: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut inside = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(s)) => {
                let name = local_name(&s);
                if name == prop {
                    inside = true;
                } else if inside
                    && name == "href"
                    && let Ok(text) = read_scalar_until(&mut reader, "href")
                {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
            Ok(Event::End(e)) if local_name_end(&e) == prop => inside = false,
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
    }
}

/// Pull `<response>` elements out of a multistatus body and turn the
/// addressbook ones into `Addressbook` records.
fn parse_addressbook_list(xml: &str, server_url: &str) -> Result<Vec<Addressbook>, UnkaiError> {
    let mut reader = Reader::from_str(xml);
    let mut books = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(s)) if local_name(&s) == "response" => {
                if let Some(book) = parse_response(&mut reader, server_url)
                    .map_err(|e| UnkaiError::Protocol(format!("CardDAV XML: {e}")))?
                {
                    books.push(book);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(UnkaiError::Protocol(format!("CardDAV XML: {e}"))),
            _ => {}
        }
    }

    // Drop Nextcloud's app-generated pseudo-addressbooks. These show up
    // in the home collection alongside real ones but aren't designed to
    // be synced by external clients — `contactsinteraction--recent` in
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
                // Transparent wrappers — descend without taking action.
                "propstat" | "prop" | "status" => {}
                "href" => href = Some(read_scalar_until(reader, "href")?),
                "resourcetype" => {
                    // Walk the resourcetype subtree looking for an
                    // <addressbook/> child (any namespace prefix).
                    // We only need to flip a flag — anything else
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
                "displayname" => display_name = Some(read_scalar_until(reader, "displayname")?),
                "getctag" => ctag = Some(read_scalar_until(reader, "getctag")?),
                "sync-token" => sync_token = Some(read_scalar_until(reader, "sync-token")?),
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
        // Same shape as SAMPLE plus a `z-app-generated--…--recent` book.
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

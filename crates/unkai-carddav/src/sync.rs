//! Incremental sync of one CardDAV addressbook via RFC 6578
//! `sync-collection` REPORT.
//!
//! # Protocol shape
//!
//! Round 1: ask "what changed since `prev_token`" â€” pass an empty
//! token on first sync to mean "give me everything".
//!
//! ```xml
//! <sync-collection xmlns="DAV:">
//!   <sync-token>{prev_token}</sync-token>
//!   <sync-level>1</sync-level>
//!   <prop>
//!     <getetag/>
//!   </prop>
//! </sync-collection>
//! ```
//!
//! Response is a multistatus with one `<response>` per changed or
//! deleted resource and a top-level `<sync-token>` we keep for next
//! time. A 200 status with an etag means added/changed; 404 means
//! deleted.
//!
//! Round 2: for the added/changed hrefs, fetch the actual vCard
//! bodies in one shot via `addressbook-multiget`. The two-phase
//! approach (sync-collection â†’ multiget) is the spec-pure way and
//! works on every server, including ones that don't inline
//! `address-data` in the sync-collection response.
//!
//! # Why two phases instead of inline `address-data`
//!
//! RFC 6352 Â§8.6.4 lets servers include `address-data` directly in
//! the sync-collection response, and Nextcloud does â€” but other
//! CardDAV servers (Radicale, Baikal historically) don't always.
//! Splitting it makes the crate work against any compliant server
//! without per-server special-casing.

use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;

use crate::client::{absolute_url, build, normalize_server_url, report};
use crate::vcard::{ParsedVcard, VcardAddress, VcardEmail, VcardPhone, parse_vcard};
use crate::xml_util::{local_name, read_text_until, skip_subtree};

/// One contact resource as seen on the server, with all the bookkeeping
/// the local cache needs: where it lives (`href`), what version it is
/// (`etag`), and the parsed vCard fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawContact {
    pub href: String,
    pub etag: String,
    pub vcard_uid: String,
    pub display_name: String,
    /// Email addresses paired with the vCard `EMAIL;TYPE=â€¦` kind.
    pub emails: Vec<VcardEmail>,
    /// Phone numbers paired with the vCard `TEL;TYPE=â€¦` kind hint.
    pub phones: Vec<VcardPhone>,
    pub organization: Option<String>,
    pub photo_mime: Option<String>,
    pub photo_data: Option<Vec<u8>>,
    /// Job title (vCard `TITLE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Birthday (vCard `BDAY`) as the literal vCard text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub birthday: Option<String>,
    /// Free-form note (vCard `NOTE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Postal addresses (vCard `ADR`). Carries the original "home"/
    /// "work"/"other" hint so the UI can group them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<VcardAddress>,
    /// Personal/work URLs (vCard `URL`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    /// Raw vCard text â€” kept so we can re-parse without re-syncing
    /// if the model evolves later.
    pub vcard_raw: String,
    /// vCard `KIND` (RFC 6350 Â§6.1.4) â€” `"group"` for a contact
    /// group / mailing list, empty for individuals.  Forwarded
    /// to the cache so the IPC layer can split groups out from
    /// regular contacts (#133, #113).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// `MEMBER` URIs for `KIND:group` cards.  Preserved verbatim
    /// so the resolver can match `urn:uuid:<uid>` references
    /// against other vCards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_uids: Vec<String>,
    /// `CATEGORIES` tag list (RFC 6350 Â§6.7.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
}

/// Result of one sync round: what to upsert, what to delete, and the
/// new sync token to persist for next time. `deleted` holds full hrefs
/// (matching `RawContact::href`) since that's how the server identifies
/// them in the response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncDelta {
    pub upserts: Vec<RawContact>,
    pub deleted_hrefs: Vec<String>,
    pub new_sync_token: Option<String>,
}

/// Sync a single addressbook.
///
/// Pass `prev_sync_token = None` (or an empty string) for the initial
/// pull. The returned `new_sync_token` should be persisted by the
/// caller so the next call can be incremental.
pub async fn sync_addressbook(
    server_url: &str,
    addressbook_url: &str,
    username: &str,
    app_password: &str,
    prev_sync_token: Option<&str>,
    trusted_certs: &[TrustedCert],
) -> Result<SyncDelta, UnkaiError> {
    let server = normalize_server_url(server_url);
    let http = build(trusted_certs)?;

    // Phase 1: sync-collection.
    let body = sync_collection_body(prev_sync_token.unwrap_or(""));
    tracing::info!(
        "CardDAV sync-collection on {addressbook_url} (token={:?})",
        prev_sync_token
    );
    let resp = report(&http, addressbook_url, username, app_password, &body).await?;
    let status = resp.status();
    // 415 means the server refuses sync-collection on this collection
    // (some Nextcloud system / app-generated addressbooks do). Treat
    // as a no-op so one quirky book doesn't break the whole sync, and
    // log so we can spot any new pseudo-books to filter at discovery.
    if status.as_u16() == 415 {
        tracing::warn!(
            "sync-collection on {addressbook_url} returned 415 â€” skipping (likely a \
             pseudo-addressbook that doesn't support sync-collection)"
        );
        return Ok(SyncDelta {
            upserts: Vec::new(),
            deleted_hrefs: Vec::new(),
            new_sync_token: None,
        });
    }
    if !status.is_success() && status.as_u16() != 207 {
        return Err(UnkaiError::Nextcloud(format!(
            "sync-collection returned HTTP {status}"
        )));
    }
    let xml = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading sync-collection body: {e}")))?;
    let parsed = parse_sync_collection(&xml, &server)
        .map_err(|e| UnkaiError::Protocol(format!("sync-collection parse: {e}")))?;

    // Phase 2: addressbook-multiget for the changed hrefs.
    let upserts = if parsed.changed.is_empty() {
        Vec::new()
    } else {
        fetch_vcards(
            &http,
            addressbook_url,
            username,
            app_password,
            &server,
            &parsed.changed,
        )
        .await?
    };

    Ok(SyncDelta {
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
                        // 404 means deleted; 200 means added/changed.
                        // Treat anything else as a soft error and skip.
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

/// Walk one `<response>` inside a sync-collection multistatus.
/// Returns `(href, status_string, etag_opt)`.
fn parse_sync_response(
    reader: &mut Reader<&[u8]>,
) -> Result<Option<(String, String, Option<String>)>, quick_xml::Error> {
    let mut href: Option<String> = None;
    let mut status: Option<String> = None;
    let mut etag: Option<String> = None;

    loop {
        match reader.read_event()? {
            Event::Start(s) => match local_name(&s).as_str() {
                // Transparent wrappers â€” descend, no state change.
                "propstat" | "prop" => {}
                "href" => href = Some(read_text_until(reader, "href")?),
                "status" => status = Some(read_text_until(reader, "status")?),
                "getetag" => etag = Some(read_text_until(reader, "getetag")?),
                other => skip_subtree(reader, other)?,
            },
            Event::End(end) if end_local(&end) == "response" => break,
            Event::Eof => break,
            _ => {}
        }
    }

    let Some(href) = href else { return Ok(None) };
    let status = status.unwrap_or_default();
    // Strip etag quotes if present â€” servers often wrap them.
    let etag = etag.map(|e| e.trim_matches('"').to_string());
    Ok(Some((href, status, etag)))
}

fn end_local(end: &quick_xml::events::BytesEnd<'_>) -> String {
    let name_owned = end.name();
    let bytes = name_owned.as_ref();
    let local = match bytes.iter().position(|&b| b == b':') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    };
    String::from_utf8_lossy(local).to_ascii_lowercase()
}

/// Phase 2: fetch the actual vCards for the hrefs we know changed.
async fn fetch_vcards(
    http: &reqwest::Client,
    addressbook_url: &str,
    username: &str,
    app_password: &str,
    server_url: &str,
    changed: &[ChangedHref],
) -> Result<Vec<RawContact>, UnkaiError> {
    let mut hrefs_xml = String::new();
    for c in changed {
        // Convert back to a server-relative path â€” multiget requires
        // the same form the server originally returned. Stripping the
        // server prefix is safe because we built the absolute form
        // from that prefix in the first place.
        let path = c.href.strip_prefix(server_url).unwrap_or(&c.href);
        hrefs_xml.push_str(&format!("  <d:href>{}</d:href>\n", xml_escape(path)));
    }

    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <d:getetag/>
    <card:address-data/>
  </d:prop>
{hrefs_xml}</card:addressbook-multiget>"#
    );

    let resp = report(http, addressbook_url, username, app_password, &body).await?;
    if !resp.status().is_success() && resp.status().as_u16() != 207 {
        return Err(UnkaiError::Nextcloud(format!(
            "addressbook-multiget returned HTTP {}",
            resp.status()
        )));
    }
    let xml = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("reading multiget body: {e}")))?;

    parse_multiget(&xml, server_url)
        .map_err(|e| UnkaiError::Protocol(format!("multiget parse: {e}")))
}

fn parse_multiget(xml: &str, server_url: &str) -> Result<Vec<RawContact>, quick_xml::Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(s) if local_name(&s) == "response" => {
                if let Some(c) = parse_multiget_response(&mut reader, server_url)? {
                    out.push(c);
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
) -> Result<Option<RawContact>, quick_xml::Error> {
    let mut href: Option<String> = None;
    let mut etag: Option<String> = None;
    let mut vcard_raw: Option<String> = None;

    loop {
        match reader.read_event()? {
            Event::Start(s) => match local_name(&s).as_str() {
                "propstat" | "prop" | "status" => {}
                "href" => href = Some(read_text_until(reader, "href")?),
                "getetag" => etag = Some(read_text_until(reader, "getetag")?),
                "address-data" => vcard_raw = Some(read_text_until(reader, "address-data")?),
                other => skip_subtree(reader, other)?,
            },
            Event::End(end) if end_local(&end) == "response" => break,
            Event::Eof => break,
            _ => {}
        }
    }

    let (Some(href), Some(etag), Some(vcard_raw)) = (href, etag, vcard_raw) else {
        return Ok(None);
    };
    let etag = etag.trim_matches('"').to_string();

    // Skip if the vCard fails to parse â€” log and move on rather than
    // failing the entire sync because of one weird record.
    let parsed: ParsedVcard = match parse_vcard(&vcard_raw) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Skipping unparseable vCard at {href}: {e}");
            return Ok(None);
        }
    };

    Ok(Some(RawContact {
        href: absolute_url(server_url, &href),
        etag,
        vcard_uid: parsed.uid,
        display_name: parsed.display_name,
        emails: parsed.emails,
        phones: parsed.phones,
        organization: parsed.organization,
        photo_mime: parsed.photo_mime,
        photo_data: parsed.photo_data,
        title: parsed.title,
        birthday: parsed.birthday,
        note: parsed.note,
        addresses: parsed.addresses,
        urls: parsed.urls,
        vcard_raw,
        kind: parsed.kind,
        member_uids: parsed.members,
        categories: parsed.categories,
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
    <d:href>/dav/ab/c1.vcf</d:href>
    <d:propstat>
      <d:prop><d:getetag>"abc"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/ab/c2.vcf</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:sync-token>http://nc/ns/sync/99</d:sync-token>
</d:multistatus>"#;
        let r = parse_sync_collection(xml, "https://cloud.example.com").unwrap();
        assert_eq!(r.changed.len(), 1);
        assert_eq!(r.changed[0].href, "https://cloud.example.com/dav/ab/c1.vcf");
        assert_eq!(r.deleted, vec!["https://cloud.example.com/dav/ab/c2.vcf"]);
        assert_eq!(r.new_sync_token.as_deref(), Some("http://nc/ns/sync/99"));
    }
}

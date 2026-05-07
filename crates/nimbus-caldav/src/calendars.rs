//! Calendar-collection operations: create, rename + recolor, delete.
//!
//! Event-level CRUD lives in `write.rs` â€” this module handles the
//! *container* it all lives in. All three ops are plain DAV requests
//! against Nextcloud's CalDAV endpoint:
//!
//! - **Create** uses the `MKCALENDAR` method (RFC 4791 Â§5.3.1).
//!   The response is an empty 201; the server picks the etag + ctag
//!   for the new collection, which the caller picks up on the next
//!   calendar-home PROPFIND sync.
//! - **Rename / recolor** use `PROPPATCH` (RFC 4918 Â§9.2) with the
//!   DAV `displayname` and/or Apple `calendar-color` properties.
//!   Both can ride in one request so "edit calendar" in the UI is
//!   a single round-trip.
//! - **Delete** reuses the existing `delete_resource` helper â€” same
//!   shape as `write::delete_event`, just pointed at the collection
//!   href instead of an event href. No `If-Match` gate: collections
//!   don't carry an etag the way individual events do.

use quick_xml::escape::escape;
use reqwest::{Method, StatusCode};

use nimbus_core::NimbusError;
use nimbus_core::models::TrustedCert;

use crate::client::{build, delete_resource};

/// Create a new calendar collection under the user's calendar home.
///
/// `calendar_home_url` is the absolute URL returned by the CalDAV
/// discovery step (e.g. `https://nc.example.com/remote.php/dav/calendars/alice/`).
/// `path_segment` is the URL-safe slug for the new collection â€”
/// callers typically generate a UUID so two concurrent creates
/// can't collide on the wire, and so renames don't rewrite URLs
/// downstream. The absolute URL we MKCALENDAR against is
/// `{calendar_home}/{path_segment}/`.
///
/// `color` is optional; when set, it's written as the Apple-namespace
/// `calendar-color` property so Nextcloud and other mainstream
/// CalDAV clients render the same swatch we show in our sidebar.
/// Format is `#RRGGBB` or `#RRGGBBAA` â€” the caller enforces that
/// shape; we pass it through.
///
/// Returns the full URL of the new collection so the caller can
/// insert a cache row immediately (the next full sync overwrites
/// with the authoritative ctag / sync-token).
pub async fn create_calendar(
    calendar_home_url: &str,
    username: &str,
    app_password: &str,
    path_segment: &str,
    display_name: &str,
    color: Option<&str>,
    trusted_certs: &[TrustedCert],
) -> Result<String, NimbusError> {
    let http = build(trusted_certs)?;
    let url = join_collection_url(calendar_home_url, path_segment);

    let body = build_mkcalendar_body(display_name, color);

    let method = Method::from_bytes(b"MKCALENDAR")
        .map_err(|e| NimbusError::Other(format!("MKCALENDAR method: {e}")))?;
    let resp = http
        .request(method, &url)
        .basic_auth(username, Some(app_password))
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body)
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("MKCALENDAR {url}: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        // 405 = "already exists on this path" â€” surface the error
        // directly rather than swallow it, so the UI can tell the
        // user to pick a different name.
        let msg = resp.text().await.unwrap_or_default();
        return Err(NimbusError::Nextcloud(format!(
            "MKCALENDAR {url} returned HTTP {status}: {msg}"
        )));
    }

    Ok(url)
}

/// PROPPATCH a calendar collection with a new display name and/or
/// color. Either (or both) may be `None`; if both are `None` we
/// short-circuit without hitting the server.
pub async fn update_calendar(
    calendar_url: &str,
    username: &str,
    app_password: &str,
    display_name: Option<&str>,
    color: Option<&str>,
    trusted_certs: &[TrustedCert],
) -> Result<(), NimbusError> {
    if display_name.is_none() && color.is_none() {
        return Ok(());
    }

    let http = build(trusted_certs)?;
    let body = build_proppatch_body(display_name, color);

    let method = Method::from_bytes(b"PROPPATCH")
        .map_err(|e| NimbusError::Other(format!("PROPPATCH method: {e}")))?;
    let resp = http
        .request(method, calendar_url)
        .basic_auth(username, Some(app_password))
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body)
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("PROPPATCH {calendar_url}: {e}")))?;

    let status = resp.status();
    // 207 Multi-Status is the normal success reply. A malformed body
    // or unknown property surfaces as a per-property `<D:status>` â€”
    // we'd need to parse the multistatus for that granularity, but
    // for the two properties we set (both well-known) Nextcloud
    // returns 207 + all-200 or a hard 4xx/5xx. The overall HTTP
    // status is a good enough gate for now.
    if !status.is_success() {
        let msg = resp.text().await.unwrap_or_default();
        return Err(NimbusError::Nextcloud(format!(
            "PROPPATCH {calendar_url} returned HTTP {status}: {msg}"
        )));
    }

    Ok(())
}

/// Delete a calendar collection at `calendar_url`. 404 is treated
/// as success (already gone) â€” same forgiving policy `delete_event`
/// uses.
pub async fn delete_calendar(
    calendar_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), NimbusError> {
    let http = build(trusted_certs)?;
    let resp = delete_resource(&http, calendar_url, username, app_password, None).await?;
    let status = resp.status();
    if !status.is_success() && status != StatusCode::NOT_FOUND {
        let msg = resp.text().await.unwrap_or_default();
        return Err(NimbusError::Nextcloud(format!(
            "DELETE {calendar_url} returned HTTP {status}: {msg}"
        )));
    }
    Ok(())
}

/// Join `calendar_home/{slug}/` with the usual trailing-slash
/// normalization. Nextcloud is picky about the trailing `/` â€” a
/// collection URL without it 301-redirects, and a PROPPATCH that
/// follows the redirect gets dropped on the floor in some proxy
/// setups.
fn join_collection_url(calendar_home: &str, slug: &str) -> String {
    let base = calendar_home.trim_end_matches('/');
    let clean = slug.trim_matches('/');
    format!("{base}/{clean}/")
}

fn build_mkcalendar_body(display_name: &str, color: Option<&str>) -> String {
    // The XML namespace block follows the RFC 4791 example nearly
    // verbatim. The Apple ns (`http://apple.com/ns/ical/`) isn't in
    // the RFC but every mainstream server + client ships support
    // for `calendar-color` there, which is why Nextcloud's web UI
    // can read / write our colour on the same wire.
    let mut props = String::new();
    props.push_str("<D:displayname>");
    props.push_str(&escape(display_name));
    props.push_str("</D:displayname>");
    if let Some(c) = color {
        props.push_str("<I:calendar-color>");
        props.push_str(&escape(c));
        props.push_str("</I:calendar-color>");
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<C:mkcalendar xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:I="http://apple.com/ns/ical/">
  <D:set>
    <D:prop>{props}</D:prop>
  </D:set>
</C:mkcalendar>"#
    )
}

fn build_proppatch_body(display_name: Option<&str>, color: Option<&str>) -> String {
    let mut props = String::new();
    if let Some(name) = display_name {
        props.push_str("<D:displayname>");
        props.push_str(&escape(name));
        props.push_str("</D:displayname>");
    }
    if let Some(c) = color {
        props.push_str("<I:calendar-color>");
        props.push_str(&escape(c));
        props.push_str("</I:calendar-color>");
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:propertyupdate xmlns:D="DAV:" xmlns:I="http://apple.com/ns/ical/">
  <D:set>
    <D:prop>{props}</D:prop>
  </D:set>
</D:propertyupdate>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_collection_url_handles_trailing_slashes() {
        assert_eq!(
            join_collection_url("https://x/dav/cal/", "work"),
            "https://x/dav/cal/work/"
        );
        assert_eq!(
            join_collection_url("https://x/dav/cal", "/work/"),
            "https://x/dav/cal/work/"
        );
    }

    #[test]
    fn mkcalendar_body_escapes_angle_brackets() {
        let body = build_mkcalendar_body("Work & <Life>", Some("#ff0000"));
        assert!(body.contains("Work &amp; &lt;Life&gt;"));
        assert!(body.contains("<I:calendar-color>#ff0000</I:calendar-color>"));
    }

    #[test]
    fn mkcalendar_body_omits_color_when_none() {
        let body = build_mkcalendar_body("Work", None);
        assert!(body.contains("<D:displayname>Work</D:displayname>"));
        assert!(!body.contains("calendar-color"));
    }

    #[test]
    fn proppatch_body_includes_only_set_properties() {
        let rename_only = build_proppatch_body(Some("New"), None);
        assert!(rename_only.contains("<D:displayname>New</D:displayname>"));
        assert!(!rename_only.contains("calendar-color"));

        let color_only = build_proppatch_body(None, Some("#00ff00"));
        assert!(!color_only.contains("displayname"));
        assert!(color_only.contains("<I:calendar-color>#00ff00</I:calendar-color>"));
    }
}

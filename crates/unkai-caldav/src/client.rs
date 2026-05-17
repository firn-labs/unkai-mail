//! Tiny HTTP layer for CalDAV: reqwest with the right headers and
//! basic auth, plus helpers for PROPFIND / REPORT.
//!
//! These two methods aren't in `reqwest::Method` (they're WebDAV
//! extensions), so we build them via `Method::from_bytes` and attach
//! headers ourselves. Mirrors the shape of `unkai-carddav::client` —
//! only the default `Content-Type` and user agent change.

use reqwest::{Client, Method, Response, StatusCode};
use std::sync::Arc;
use std::time::Duration;

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;
use unkai_core::tls::build_client_config;
use unkai_core::url::ensure_https;

/// Build the shared HTTP client.
///
/// `trusted_certs` is the per-account self-signed-cert trust list
/// (#253) — empty for the public-CA case, populated for self-hosted
/// servers whose TLS cert isn't in webpki-roots.  Plumbed through
/// the same `build_client_config` helper IMAP/SMTP use, so the
/// fingerprint-fallback verifier covers HTTPS too.
pub fn build(trusted_certs: &[TrustedCert]) -> Result<Client, UnkaiError> {
    let rustls_config = Arc::unwrap_or_clone(build_client_config(trusted_certs));
    Client::builder()
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("Unkai Mail CalDAV/", env!("CARGO_PKG_VERSION")))
        .use_preconfigured_tls(rustls_config)
        .build()
        .map_err(|e| UnkaiError::Network(format!("failed to build CalDAV HTTP client: {e}")))
}

/// PROPFIND with a given depth and XML body.
///
/// Depth `0` queries the resource itself; `1` queries it and direct
/// children. CalDAV calendar-home listing wants `1` (the home plus
/// each calendar collection).
pub async fn propfind(
    http: &Client,
    url: &str,
    username: &str,
    app_password: &str,
    depth: u32,
    body: &str,
) -> Result<Response, UnkaiError> {
    ensure_https(url)?;
    let method = Method::from_bytes(b"PROPFIND")
        .map_err(|e| UnkaiError::Other(format!("PROPFIND method: {e}")))?;
    http.request(method, url)
        .basic_auth(username, Some(app_password))
        .header("Depth", depth.to_string())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("PROPFIND {url}: {e}")))
}

/// REPORT against a calendar collection. `Depth: 1` is what every
/// calendar-scoped report (sync-collection, calendar-multiget,
/// calendar-query) wants — the collection plus its members.
pub async fn report(
    http: &Client,
    url: &str,
    username: &str,
    app_password: &str,
    body: &str,
) -> Result<Response, UnkaiError> {
    ensure_https(url)?;
    let method = Method::from_bytes(b"REPORT")
        .map_err(|e| UnkaiError::Other(format!("REPORT method: {e}")))?;
    http.request(method, url)
        .basic_auth(username, Some(app_password))
        .header("Depth", "1")
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("REPORT {url}: {e}")))
}

/// PUT a `text/calendar` body to a calendar resource.
///
/// `if_match` carries the existing etag for an update — the server
/// returns 412 if the resource changed under us. For a fresh create,
/// pass `None` and set `if_none_match_star = true` so the PUT only
/// succeeds when the href is unused (basic two-client safety).
pub async fn put_ics(
    http: &Client,
    url: &str,
    username: &str,
    app_password: &str,
    body: &str,
    if_match: Option<&str>,
    if_none_match_star: bool,
) -> Result<Response, UnkaiError> {
    ensure_https(url)?;
    let mut req = http
        .put(url)
        .basic_auth(username, Some(app_password))
        .header("Content-Type", "text/calendar; charset=utf-8")
        .body(body.to_string());
    if let Some(etag) = if_match {
        let v = if etag.starts_with('"') {
            etag.to_string()
        } else {
            format!("\"{etag}\"")
        };
        req = req.header("If-Match", v);
    }
    if if_none_match_star {
        req = req.header("If-None-Match", "*");
    }
    req.send()
        .await
        .map_err(|e| UnkaiError::Network(format!("PUT {url}: {e}")))
}

/// DELETE a CalDAV resource at `url`. `if_match` is recommended (and
/// Nextcloud requires it) so we don't blow away an event someone else
/// just edited.
pub async fn delete_resource(
    http: &Client,
    url: &str,
    username: &str,
    app_password: &str,
    if_match: Option<&str>,
) -> Result<Response, UnkaiError> {
    delete_resource_inner(http, url, username, app_password, if_match, false).await
}

/// DELETE variant that suppresses Sabre/DAV's auto-iTIP
/// dispatch via the `Schedule-Reply: F` header (RFC 6638 §8.1).
/// Used by the "Remove from my calendar" flow on a cancelled
/// meeting: the organiser already sent CANCEL, the user is
/// just cleaning up their local copy, and Sabre's default
/// attendee-side DELETE behaviour would emit a
/// `METHOD:REPLY;PARTSTAT=DECLINED` to the organiser — noise
/// (and confusing — the organiser cancelled, why is the
/// attendee declining?).  Suppressing it keeps the operation
/// silent.
pub async fn delete_resource_no_itip(
    http: &Client,
    url: &str,
    username: &str,
    app_password: &str,
    if_match: Option<&str>,
) -> Result<Response, UnkaiError> {
    delete_resource_inner(http, url, username, app_password, if_match, true).await
}

async fn delete_resource_inner(
    http: &Client,
    url: &str,
    username: &str,
    app_password: &str,
    if_match: Option<&str>,
    suppress_itip: bool,
) -> Result<Response, UnkaiError> {
    ensure_https(url)?;
    let mut req = http.delete(url).basic_auth(username, Some(app_password));
    if let Some(etag) = if_match {
        let v = if etag.starts_with('"') {
            etag.to_string()
        } else {
            format!("\"{etag}\"")
        };
        req = req.header("If-Match", v);
    }
    if suppress_itip {
        req = req.header("Schedule-Reply", "F");
    }
    req.send()
        .await
        .map_err(|e| UnkaiError::Network(format!("DELETE {url}: {e}")))
}

/// Definitive read-only probe — PUT a tiny placeholder VEVENT,
/// observe what the server says, and DELETE it on the way out.
///
/// Why an active probe instead of OPTIONS / privilege-set:
///   Both header- and prop-based signals lie on at least one
///   real Sabre/DAV configuration we've seen — `Allow:` returns
///   the resource-type's full method list regardless of ACL,
///   and `<current-user-privilege-set>` either isn't advertised
///   or comes back claiming write privileges that the actual
///   PUT then refuses with 404.  The only signal that matches
///   what the user will hit at save-time is an actual PUT.
///
/// Probe shape:
///   - UID `unkai-readonly-probe-{uuid}` so the resource is
///     uniquely owned by us and recognisable in any orphaned
///     state.
///   - DTSTART / DTEND fixed at `19700101T000000Z` so the event
///     never surfaces in real date-range queries even if the
///     cleanup DELETE silently fails.
///   - SUMMARY says "Unkai read-only probe (auto-deleted)" so
///     a human staring at server logs / orphaned data sees
///     immediately what it is.
///   - No ATTENDEE / ORGANIZER → Sabre/DAV's auto-iTIP never
///     fires, so we never leak a probe-event invitation.
///
/// Returns:
///   - `Ok(true)` — PUT succeeded → calendar is writable.
///     The cleanup DELETE is fired right after; failures there
///     are logged but do not affect the verdict.
///   - `Ok(false)` — PUT returned 403 Forbidden or 404 Not
///     Found.  Sabre/DAV (NC's CalDAV stack) returns 404
///     instead of 403 for ACL-denied resources as a
///     permission-masking pattern, so we treat both as the
///     same signal.
///   - `Err(_)` — probe itself failed (network, auth, HTTP 5xx,
///     unexpected status).  Caller should leave the existing
///     `read_only` flag alone rather than misclassify on a
///     transient blip.
pub async fn probe_calendar_writable(
    calendar_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<bool, UnkaiError> {
    let http = build(trusted_certs)?;
    let probe_uid = format!("unkai-readonly-probe-{}", uuid::Uuid::new_v4());
    let probe_href = format!("{}/{}.ics", calendar_url.trim_end_matches('/'), probe_uid);
    let ics = format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//Unkai Mail//read-only probe//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:{probe_uid}\r\n\
         DTSTAMP:19700101T000000Z\r\n\
         DTSTART:19700101T000000Z\r\n\
         DTEND:19700101T010000Z\r\n\
         SUMMARY:Unkai read-only probe (auto-deleted)\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    );

    let put = put_ics(&http, &probe_href, username, app_password, &ics, None, true).await?;
    let status = put.status();
    tracing::debug!(
        "CalDAV read-only probe PUT {} → HTTP {}",
        probe_href,
        status
    );

    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "read-only probe PUT {probe_href} returned HTTP {status}"
        )));
    }

    // The PUT succeeded.  Capture the etag (some servers gate
    // DELETE on If-Match even when we just created the resource)
    // and clean up.  Any failure here is logged but not
    // propagated — the verdict is "writable" regardless of how
    // tidy the cleanup turned out.
    let etag = put
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string());
    match delete_resource(&http, &probe_href, username, app_password, etag.as_deref()).await {
        Ok(resp) => {
            let dstatus = resp.status();
            if !dstatus.is_success() && dstatus != StatusCode::NOT_FOUND {
                tracing::warn!(
                    "CalDAV read-only probe: cleanup DELETE {probe_href} returned HTTP {dstatus} \
                     (probe event may need manual removal)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                "CalDAV read-only probe: cleanup DELETE {probe_href} failed: {e} \
                 (probe event may need manual removal)"
            );
        }
    }

    Ok(true)
}

/// Strip a trailing `/` from a server URL.
pub fn normalize_server_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Resolve a possibly-relative `href` from a multistatus response
/// against the server's base URL. CalDAV servers usually return
/// absolute paths (no scheme/host), occasionally full URLs.
pub fn absolute_url(server_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{}{}", normalize_server_url(server_url), href)
    } else {
        format!("{}/{}", normalize_server_url(server_url), href)
    }
}

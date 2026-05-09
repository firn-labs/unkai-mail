//! Tiny HTTP layer for CalDAV: reqwest with the right headers and
//! basic auth, plus helpers for PROPFIND / REPORT.
//!
//! These two methods aren't in `reqwest::Method` (they're WebDAV
//! extensions), so we build them via `Method::from_bytes` and attach
//! headers ourselves. Mirrors the shape of `nimbus-carddav::client` —
//! only the default `Content-Type` and user agent change.

use reqwest::{Client, Method, Response};
use std::sync::Arc;
use std::time::Duration;

use nimbus_core::NimbusError;
use nimbus_core::models::TrustedCert;
use nimbus_core::tls::build_client_config;
use nimbus_core::url::ensure_https;

/// Build the shared HTTP client.
///
/// `trusted_certs` is the per-account self-signed-cert trust list
/// (#253) — empty for the public-CA case, populated for self-hosted
/// servers whose TLS cert isn't in webpki-roots.  Plumbed through
/// the same `build_client_config` helper IMAP/SMTP use, so the
/// fingerprint-fallback verifier covers HTTPS too.
pub fn build(trusted_certs: &[TrustedCert]) -> Result<Client, NimbusError> {
    let rustls_config = Arc::unwrap_or_clone(build_client_config(trusted_certs));
    Client::builder()
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("Nimbus Mail CalDAV/", env!("CARGO_PKG_VERSION")))
        .use_preconfigured_tls(rustls_config)
        .build()
        .map_err(|e| NimbusError::Network(format!("failed to build CalDAV HTTP client: {e}")))
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
) -> Result<Response, NimbusError> {
    ensure_https(url)?;
    let method = Method::from_bytes(b"PROPFIND")
        .map_err(|e| NimbusError::Other(format!("PROPFIND method: {e}")))?;
    http.request(method, url)
        .basic_auth(username, Some(app_password))
        .header("Depth", depth.to_string())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("PROPFIND {url}: {e}")))
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
) -> Result<Response, NimbusError> {
    ensure_https(url)?;
    let method = Method::from_bytes(b"REPORT")
        .map_err(|e| NimbusError::Other(format!("REPORT method: {e}")))?;
    http.request(method, url)
        .basic_auth(username, Some(app_password))
        .header("Depth", "1")
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("REPORT {url}: {e}")))
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
) -> Result<Response, NimbusError> {
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
        .map_err(|e| NimbusError::Network(format!("PUT {url}: {e}")))
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
) -> Result<Response, NimbusError> {
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
) -> Result<Response, NimbusError> {
    delete_resource_inner(http, url, username, app_password, if_match, true).await
}

async fn delete_resource_inner(
    http: &Client,
    url: &str,
    username: &str,
    app_password: &str,
    if_match: Option<&str>,
    suppress_itip: bool,
) -> Result<Response, NimbusError> {
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
        .map_err(|e| NimbusError::Network(format!("DELETE {url}: {e}")))
}

/// OPTIONS probe — returns `true` iff the server's `Allow` header
/// advertises `PUT` (or `DELETE`) on the calendar collection.
///
/// We use this as a belt-and-braces companion to the
/// `current-user-privilege-set` PROPFIND in `discovery`: some Sabre/DAV
/// builds either omit the privilege set entirely on shared calendars or
/// shape it in a way our parser misses, but every CalDAV server has to
/// answer OPTIONS truthfully (RFC 4918 §18) so the calendar-edit UI
/// can fall back to this signal. Callers run it during
/// `sync_nextcloud_calendars` and stamp the verdict onto the cached
/// `read_only` flag so the EventEditor can grey itself out before the
/// user even tries to write.
///
/// `Ok(true)` means the calendar accepts writes; `Ok(false)` means it
/// answered cleanly but refused PUT/DELETE; `Err(_)` means the probe
/// itself failed (network, auth) and the caller should leave the
/// existing `read_only` flag alone.
pub async fn calendar_is_writable(
    calendar_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<bool, NimbusError> {
    ensure_https(calendar_url)?;
    let http = build(trusted_certs)?;
    let resp = http
        .request(Method::OPTIONS, calendar_url)
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("OPTIONS {calendar_url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "OPTIONS {calendar_url} returned HTTP {}",
            resp.status()
        )));
    }
    let allow = resp
        .headers()
        .get(reqwest::header::ALLOW)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_uppercase();
    // Logging the raw `Allow` header is the only way to diagnose
    // servers where the OPTIONS probe disagrees with the actual
    // PUT verdict (e.g. Sabre/DAV configs that return the full
    // resource-type method list regardless of ACL).  `info!` so
    // it shows up at the default level — the volume per sync is
    // one line per calendar, manageable.
    tracing::info!("CalDAV OPTIONS {}: Allow={:?}", calendar_url, allow);
    // Empty `Allow` → server didn't tell us, assume writable so we
    // don't accidentally lock the user out of every calendar.
    if allow.is_empty() {
        return Ok(true);
    }
    Ok(allow.contains("PUT") || allow.contains("DELETE"))
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

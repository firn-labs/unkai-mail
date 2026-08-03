//! Tiny HTTP layer for CardDAV: reqwest with the right headers and
//! basic auth, and a couple of helpers for sending PROPFIND / REPORT.
//!
//! These two methods are not in `reqwest::Method` (they're WebDAV
//! extensions), so we build the requests via `Method::from_bytes` and
//! attach the body / headers ourselves.

use reqwest::{Client, Method, Response};
use std::sync::Arc;
use std::time::Duration;

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;
use unkai_core::tls::build_client_config;
use unkai_core::url::ensure_https;

/// Build the shared HTTP client.
///
/// Same shape as the Nextcloud one — generous timeouts because some
/// self-hosted servers answer slowly under load, and a recognisable
/// user agent so server admins can spot us in logs.
///
/// `trusted_certs` is the per-account self-signed-cert trust list
/// (#253), plumbed through the shared `build_client_config` helper
/// so HTTPS to a self-hosted CardDAV server with a non-public CA
/// goes through the same fingerprint-fallback verifier IMAP/SMTP use.
pub fn build(trusted_certs: &[TrustedCert]) -> Result<Client, UnkaiError> {
    let rustls_config = Arc::unwrap_or_clone(build_client_config(trusted_certs));
    Client::builder()
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("Unkai Mail CardDAV/", env!("CARGO_PKG_VERSION")))
        .use_preconfigured_tls(rustls_config)
        .build()
        .map_err(|e| UnkaiError::Network(format!("failed to build CardDAV HTTP client: {e}")))
}

/// Same client as [`build`] but with automatic redirect following
/// turned off (#413).
///
/// reqwest's default policy rewrites a redirected PROPFIND into a
/// GET (per the usual 301/302/303 browser semantics), which breaks
/// the RFC 6764 `/.well-known/carddav` bootstrap — those endpoints
/// answer with a redirect to the real DAV context path and expect
/// the client to repeat the *same* method there. Discovery follows
/// the hops manually instead.
pub fn build_no_redirect(trusted_certs: &[TrustedCert]) -> Result<Client, UnkaiError> {
    let rustls_config = Arc::unwrap_or_clone(build_client_config(trusted_certs));
    Client::builder()
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("Unkai Mail CardDAV/", env!("CARGO_PKG_VERSION")))
        .use_preconfigured_tls(rustls_config)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| UnkaiError::Network(format!("failed to build CardDAV HTTP client: {e}")))
}

/// PROPFIND with a given depth and XML body.
///
/// `depth = 0` queries the resource itself, `1` queries it and direct
/// children. CardDAV addressbook listing wants 1 (the home + each book).
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

/// REPORT with the standard CardDAV body. `Depth: 1` is what every
/// addressbook-scoped report wants (the collection + its members).
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

/// PUT a vCard to its addressbook URL.
///
/// `if_match` is the current etag for an update (servers reject the
/// PUT with 412 if the resource has moved on under us — basic
/// optimistic concurrency). For a fresh create, pass `None`; the
/// server treats it as "create only if absent" when paired with
/// `If-None-Match: *` instead, which we also do via `if_none_match`.
pub async fn put_vcard(
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
        .header("Content-Type", "text/vcard; charset=utf-8")
        .body(body.to_string());
    if let Some(etag) = if_match {
        // Etags travel quoted on the wire — wrap if the caller didn't.
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

/// DELETE a CardDAV resource at `url`.
///
/// `if_match` is recommended (and Nextcloud requires it) so we don't
/// blow away a contact someone else just edited.
pub async fn delete_resource(
    http: &Client,
    url: &str,
    username: &str,
    app_password: &str,
    if_match: Option<&str>,
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
    req.send()
        .await
        .map_err(|e| UnkaiError::Network(format!("DELETE {url}: {e}")))
}

/// Strip a trailing `/` from a server URL — same helper as in
/// `unkai-nextcloud::client`. Duplicated rather than depended upon to
/// keep this crate from pulling in the whole NC integration.
pub fn normalize_server_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Resolve a possibly-relative `href` from a multistatus response
/// against the server's base URL. CardDAV servers usually return
/// absolute paths (no scheme/host), occasionally full URLs.
pub fn absolute_url(server_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{}{}", normalize_server_url(server_url), href)
    } else {
        // Defensive: treat as relative to root.
        format!("{}/{}", normalize_server_url(server_url), href)
    }
}

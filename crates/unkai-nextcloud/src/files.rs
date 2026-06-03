//! Nextcloud Files (WebDAV) integration — browse + download.
//!
//! # What this module does
//!
//! Two operations, enough to power the "attach from Nextcloud" flow:
//!
//! - `list_directory` — PROPFIND at depth 1 on a folder, returning a
//!   flat listing of its children.
//! - `download_file` — GET a file's bytes.
//!
//! # Endpoint shape
//!
//! Nextcloud exposes WebDAV at:
//!
//! ```text
//!   {server}/remote.php/dav/files/{username}/{path}
//! ```
//!
//! `{path}` is the folder or file path relative to the user's root.
//! The trailing-slash convention matters: WebDAV treats `/foo` and
//! `/foo/` differently on some servers (the latter is unambiguously a
//! collection). We always send a trailing slash on folder PROPFINDs
//! to keep Nextcloud happy.
//!
//! # Path encoding
//!
//! WebDAV URLs are real URLs — spaces and unicode in filenames must be
//! percent-encoded per segment. We split the user-supplied path on `/`,
//! encode each segment, and rejoin. Encoding the whole string as one
//! blob would escape the slashes and break the URL.
//!
//! # Why not a full WebDAV crate
//!
//! The ecosystem's `webdav`/`reqwest_dav` crates pull in their own HTTP
//! clients, their own auth stacks, and a lot of API we don't need.
//! PROPFIND + GET is a dozen lines of reqwest and a small XML parser;
//! staying hand-rolled keeps the dep graph small and leaves us in full
//! control of headers (`OCS-APIRequest`, UA).

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;

use crate::client;

/// One entry in a Nextcloud directory listing.
///
/// `path` is the full path relative to the user's root, with a leading
/// `/` and (for directories) a trailing `/`. The UI uses this as the
/// canonical identifier when navigating or downloading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Display name (last segment of the path, percent-decoded).
    pub name: String,
    /// Full path under the user's root, e.g. `/Documents/report.pdf`.
    /// Directory paths end with `/`.
    pub path: String,
    /// True for folders (DAV collections), false for regular files.
    pub is_dir: bool,
    /// File size in bytes. `None` for directories (DAV's
    /// `<getcontentlength>` is only defined for non-collections).
    pub size: Option<u64>,
    /// MIME type from `<getcontenttype>`. `None` for directories.
    pub content_type: Option<String>,
    /// Last-modified timestamp from `<getlastmodified>` (HTTP-date).
    pub modified: Option<DateTime<Utc>>,
}

/// Build the per-user WebDAV base URL without a trailing slash:
/// `https://cloud.example.com/remote.php/dav/files/alice`.
fn user_dav_base(server_url: &str, username: &str) -> String {
    let server = client::normalize_server_url(server_url);
    format!(
        "{server}/remote.php/dav/files/{}",
        encode_path_segment(username)
    )
}

/// Percent-encode a single path segment.
///
/// Nextcloud filenames can contain spaces, `#`, `?`, unicode — anything
/// not unreserved per RFC 3986 needs escaping. We encode everything
/// that's not unreserved/sub-delims (minus `/`, since this function
/// handles a *single* segment). `/` inside a name would be impossible
/// on most filesystems anyway; if it appears, it'll be escaped as
/// `%2F` and land as a single segment on the server.
fn encode_path_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            other => {
                // Two-hex-digit percent escape.
                let _ = write!(&mut out, "%{other:02X}");
            }
        }
    }
    out
}

/// Encode each segment of a path but preserve the `/` separators so the
/// result is still a valid URL path.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Reverse of `encode_path_segment` — used when we parse an `<href>` out
/// of a multistatus response and want the human-readable name back.
fn decode_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Normalise a caller-supplied path into the `/foo/bar/` form we send
/// to the server. Accepts `""`, `"/"`, `"Documents"`, `"/Documents/"` —
/// all end up as valid paths. Trailing slash is *added* for folder
/// PROPFINDs by the caller, not here.
fn normalise_input_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        String::from("/")
    } else {
        format!("/{trimmed}")
    }
}

// ── PROPFIND body ──────────────────────────────────────────────

/// Minimal PROPFIND body asking only for the four props we actually
/// render. Nextcloud is happy to send more, but we save bandwidth and
/// parser time by narrowing the request.
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <d:getcontentlength/>
    <d:getcontenttype/>
    <d:getlastmodified/>
  </d:prop>
</d:propfind>"#;

/// List the immediate children of `path` on the given Nextcloud server.
///
/// `path` is relative to the user's root. An empty string or `/` means
/// the root folder. The returned list does **not** include the folder
/// itself — PROPFIND at depth 1 echoes the request target as its first
/// response, but we filter it out so the caller sees only children.
///
/// # Errors
/// - `UnkaiError::Auth` if the app password is rejected (401).
/// - `UnkaiError::Nextcloud` for any other non-2xx response.
/// - `UnkaiError::Protocol` if the XML doesn't parse.
pub async fn list_directory(
    server_url: &str,
    username: &str,
    app_password: &str,
    path: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<FileEntry>, UnkaiError> {
    let base = user_dav_base(server_url, username);
    let inner = normalise_input_path(path);
    // For folders we want a trailing slash; Nextcloud redirects
    // collection GET/PROPFIND on a path without it, which wastes a round
    // trip and confuses basic auth replay on some servers.
    let folder_path = if inner == "/" {
        "/".to_string()
    } else {
        format!("{inner}/")
    };
    let url = format!("{base}{}", encode_path(&folder_path));

    tracing::debug!("PROPFIND {url}");

    let http = client::build(trusted_certs)?;
    let resp = http
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid HTTP method"),
            &url,
        )
        .header("OCS-APIRequest", "true")
        .header(CONTENT_TYPE, "application/xml; charset=utf-8")
        .header("Depth", "1")
        .basic_auth(username, Some(app_password))
        .body(PROPFIND_BODY)
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("PROPFIND request failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(UnkaiError::Nextcloud(format!(
            "Nextcloud path not found: {path}"
        )));
    }
    // 207 Multi-Status is the expected success code; some servers send
    // 200 on empty collections — accept anything 2xx.
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "PROPFIND returned HTTP {status}"
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("PROPFIND body read failed: {e}")))?;

    let entries = parse_multistatus(&body, username, &folder_path)
        .map_err(|e| UnkaiError::Protocol(format!("PROPFIND XML parse failed: {e}")))?;
    Ok(entries)
}

/// Download a file's raw bytes.
///
/// `path` is the file path relative to the user's root. Folders will
/// return a 405/207 depending on the server; callers should only
/// invoke this on entries with `is_dir: false`.
pub async fn download_file(
    server_url: &str,
    username: &str,
    app_password: &str,
    path: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<u8>, UnkaiError> {
    let base = user_dav_base(server_url, username);
    let inner = normalise_input_path(path);
    let url = format!("{base}{}", encode_path(&inner));

    tracing::debug!("GET {url}");

    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("file GET failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(UnkaiError::Nextcloud(format!(
            "Nextcloud file not found: {path}"
        )));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "file GET returned HTTP {status}"
        )));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| UnkaiError::Network(format!("file body read failed: {e}")))?;
    Ok(bytes.to_vec())
}

/// Fetch a server-rendered preview thumbnail of a file via
/// `/index.php/core/preview.png?file=<path>&x=…&y=…`.  Used by
/// the Nextcloud file picker to render an inline thumbnail on
/// image and video rows.
///
/// `path` is relative to the user's root (the same form
/// `download_file` takes).  `size` caps the long edge in pixels;
/// Nextcloud renders a smaller image to fit and we let the
/// browser scale it down further.  Returns the raw bytes — the
/// caller decides whether to base64 it for a `data:` URL or
/// stream it through a custom URI scheme.
///
/// Errors mirror `download_file`: 401 → `Auth`, 404 → `Nextcloud`,
/// other non-2xx → `Nextcloud`.  Files with no available preview
/// (e.g. ones the server hasn't generated thumbnails for yet)
/// surface as 404 too; the caller treats that as "skip preview,
/// fall back to the typed icon".
pub async fn fetch_preview(
    server_url: &str,
    username: &str,
    app_password: &str,
    path: &str,
    size: u32,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<u8>, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let inner = normalise_input_path(path);
    // The preview endpoint accepts the path relative to the
    // user's root in the `file` query parameter.  `forceIcon=0`
    // means "return 404 if no preview exists" rather than serving
    // the generic mimetype icon, so we know to fall back.
    // `a=1` keeps aspect ratio so portraits don't get cropped.
    // `mimeFallback=false` is explicit insurance against future
    // NC versions flipping the default — we want a real preview
    // or a 404, never a mime-typed icon.  `forceIcon=0` belongs
    // to the same family.
    let url = format!(
        "{server}/index.php/core/preview.png?file={}&x={size}&y={size}&a=1&forceIcon=0&mimeFallback=false",
        encode_path(&inner),
    );

    tracing::debug!("GET preview {url}");

    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("preview GET failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(UnkaiError::Nextcloud(format!(
            "no preview available for {path}"
        )));
    }
    if !status.is_success() {
        tracing::warn!("preview GET {url} returned HTTP {status}");
        return Err(UnkaiError::Nextcloud(format!(
            "preview GET returned HTTP {status}"
        )));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| UnkaiError::Network(format!("preview body read failed: {e}")))?;
    Ok(bytes.to_vec())
}

/// Upload (or overwrite) a file via WebDAV PUT.
///
/// `path` is the destination, relative to the user's root — e.g.
/// `/Documents/invoice.pdf`. The parent folder must already exist.
/// `bytes` is the raw file content. `content_type` is advisory: if set
/// it goes in the request header so Nextcloud records a sensible MIME
/// type; when unset we fall back to `application/octet-stream`.
///
/// On success, returns the full path Nextcloud accepted (same as
/// `path`). If a file already exists at that path WebDAV PUT overwrites
/// it — callers that need to avoid clobbering should check first or
/// append a suffix to the filename.
///
/// # Errors
/// - `UnkaiError::Auth` — app password rejected (401).
/// - `UnkaiError::Nextcloud` — parent folder missing (409), quota
///   exceeded (507), or any other non-2xx response.
pub async fn upload_file(
    server_url: &str,
    username: &str,
    app_password: &str,
    path: &str,
    bytes: Vec<u8>,
    content_type: Option<&str>,
    trusted_certs: &[TrustedCert],
) -> Result<String, UnkaiError> {
    let base = user_dav_base(server_url, username);
    let inner = normalise_input_path(path);
    if inner == "/" {
        return Err(UnkaiError::Nextcloud(
            "refusing to PUT to the user root".into(),
        ));
    }
    let url = format!("{base}{}", encode_path(&inner));

    tracing::debug!("PUT {url} ({} bytes)", bytes.len());

    let http = client::build(trusted_certs)?;
    let resp = http
        .put(&url)
        .header("OCS-APIRequest", "true")
        .header(
            CONTENT_TYPE,
            content_type.unwrap_or("application/octet-stream"),
        )
        .basic_auth(username, Some(app_password))
        .body(bytes)
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("PUT request failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    if status == reqwest::StatusCode::CONFLICT {
        return Err(UnkaiError::Nextcloud(format!(
            "parent folder missing for: {path}"
        )));
    }
    // 507 Insufficient Storage = user's Nextcloud quota is full. Worth
    // calling out specifically so the UI can say something more useful
    // than "HTTP 507".
    if status == reqwest::StatusCode::INSUFFICIENT_STORAGE {
        return Err(UnkaiError::Nextcloud(
            "Nextcloud quota exceeded — file not saved".into(),
        ));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!("PUT returned HTTP {status}")));
    }
    Ok(inner)
}

/// Create a new (empty) folder via WebDAV MKCOL.
///
/// `path` is the full path of the folder to create, relative to the
/// user's root (e.g. `/Documents/New Folder`). The parent must already
/// exist — MKCOL will not create intermediate directories. Trailing
/// slash is optional; we add one before sending so Nextcloud unambiguously
/// treats the target as a collection.
///
/// # Errors
/// - `UnkaiError::Auth` — app password rejected (401).
/// - `UnkaiError::Nextcloud` — folder already exists (405), parent
///   missing (409), or any other non-2xx response. The HTTP status is
///   included so the UI can show something specific.
pub async fn create_directory(
    server_url: &str,
    username: &str,
    app_password: &str,
    path: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    let base = user_dav_base(server_url, username);
    let inner = normalise_input_path(path);
    if inner == "/" {
        // Refuse to MKCOL the user root — Nextcloud already created it
        // at signup, and a 405 here would just be noise.
        return Err(UnkaiError::Nextcloud(
            "cannot create the root folder".into(),
        ));
    }
    let folder_path = format!("{inner}/");
    let url = format!("{base}{}", encode_path(&folder_path));

    tracing::debug!("MKCOL {url}");

    let http = client::build(trusted_certs)?;
    let resp = http
        .request(
            reqwest::Method::from_bytes(b"MKCOL").expect("MKCOL is a valid HTTP method"),
            &url,
        )
        .header("OCS-APIRequest", "true")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("MKCOL request failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    // 405 = collection already exists at that path. Surface it as a
    // distinct, recognisable message so the UI can prompt the user to
    // pick a different name rather than a generic "HTTP 405".
    if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        return Err(UnkaiError::Nextcloud(format!(
            "folder already exists: {path}"
        )));
    }
    // 409 = parent missing. With the picker driving folder creation we
    // shouldn't ever hit this (the user has just navigated *into* the
    // parent), but a clear message helps if it does.
    if status == reqwest::StatusCode::CONFLICT {
        return Err(UnkaiError::Nextcloud(format!(
            "parent folder missing for: {path}"
        )));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "MKCOL returned HTTP {status}"
        )));
    }
    Ok(())
}

/// DAV DELETE the resource at `path` — works for both files and
/// folders. Used by the Office viewer cleanup flow + the temp-dir
/// sweeper. Treats 404 as success (already gone), same forgiving
/// policy `unkai-caldav::delete_calendar` uses.
pub async fn delete_path(
    server_url: &str,
    username: &str,
    app_password: &str,
    path: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    let base = user_dav_base(server_url, username);
    let inner = normalise_input_path(path);
    if inner == "/" {
        return Err(UnkaiError::Nextcloud(
            "refusing to DELETE the user root".into(),
        ));
    }
    let url = format!("{base}{}", encode_path(&inner));
    tracing::debug!("DELETE {url}");

    let http = client::build(trusted_certs)?;
    let resp = http
        .delete(&url)
        .header("OCS-APIRequest", "true")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("DELETE request failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    // 404 = already gone, 204/200 = success — all fine.
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "DELETE returned HTTP {status}"
        )));
    }
    Ok(())
}

/// Single-resource PROPFIND that returns the Nextcloud `oc:fileid` —
/// the stable numeric handle every NC app keys on. Used by the
/// Office viewer flow to build the `index.php/f/<fileid>` deep-link
/// URL after a fresh upload (the new file's fileid isn't returned
/// by PUT).
pub async fn propfind_fileid(
    server_url: &str,
    username: &str,
    app_password: &str,
    path: &str,
    trusted_certs: &[TrustedCert],
) -> Result<String, UnkaiError> {
    let base = user_dav_base(server_url, username);
    let inner = normalise_input_path(path);
    let url = format!("{base}{}", encode_path(&inner));

    // Depth 0 — we only care about the resource itself, not children.
    // The `oc:` namespace is Nextcloud's own; `oc:fileid` is the
    // numeric id Files / Office / Talk all share.
    const BODY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:prop>
    <oc:fileid/>
  </d:prop>
</d:propfind>"#;

    let http = client::build(trusted_certs)?;
    let resp = http
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid HTTP method"),
            &url,
        )
        .header("OCS-APIRequest", "true")
        .header(CONTENT_TYPE, "application/xml; charset=utf-8")
        .header("Depth", "0")
        .basic_auth(username, Some(app_password))
        .body(BODY)
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("PROPFIND fileid failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "PROPFIND fileid returned HTTP {status}"
        )));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("PROPFIND body read failed: {e}")))?;

    // Cheap text scan rather than a full quick-xml read — the
    // response is ~600 bytes and we only want one element.
    let open = body.find("<oc:fileid>").or_else(|| body.find("<fileid>"));
    let close = body.find("</oc:fileid>").or_else(|| body.find("</fileid>"));
    match (open, close) {
        (Some(o), Some(c)) if c > o => {
            let start = o + body[o..].find('>').unwrap_or(0) + 1;
            Ok(body[start..c].trim().to_string())
        }
        _ => Err(UnkaiError::Protocol(
            "PROPFIND response missing <oc:fileid>".into(),
        )),
    }
}

// ── Multistatus parser ─────────────────────────────────────────

/// Parse a WebDAV multistatus response into `FileEntry`s.
///
/// The shape is:
///
/// ```xml
/// <d:multistatus>
///   <d:response>
///     <d:href>/remote.php/dav/files/alice/Documents/</d:href>
///     <d:propstat>
///       <d:prop>
///         <d:displayname>Documents</d:displayname>
///         <d:resourcetype><d:collection/></d:resourcetype>
///         <d:getlastmodified>Tue, 21 Apr 2026 10:00:00 GMT</d:getlastmodified>
///         <d:getcontentlength>…</d:getcontentlength>  <!-- files only -->
///         <d:getcontenttype>…</d:getcontenttype>      <!-- files only -->
///       </d:prop>
///       <d:status>HTTP/1.1 200 OK</d:status>
///     </d:propstat>
///   </d:response>
///   ... more responses ...
/// </d:multistatus>
/// ```
///
/// We drop the response whose href matches the request target (the
/// folder itself) so callers only see children.
///
/// Implementation note: we reuse the same "strip namespace prefix,
/// match local name" approach as the CardDAV crate's `xml_util`. This
/// crate is small enough that inlining the two helpers we need keeps
/// `unkai-nextcloud` free of an internal dep on carddav.
fn parse_multistatus(
    body: &str,
    username: &str,
    request_path: &str,
) -> Result<Vec<FileEntry>, quick_xml::Error> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    // Prefix of the href we should treat as "our path", so we can
    // strip it and produce user-facing paths relative to the user root.
    // Nextcloud returns hrefs URL-encoded; we match encoded-to-encoded.
    let user_prefix = format!("/remote.php/dav/files/{}", encode_path_segment(username));
    // Encoded form of the request target under the user root.
    let request_prefix_encoded = encode_path(request_path); // e.g. "/Documents/"

    let mut entries = Vec::new();
    let mut current: Option<PartialEntry> = None;

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let local = local_name(&e);
                match local.as_str() {
                    "response" => current = Some(PartialEntry::default()),
                    "href" => {
                        if let Some(entry) = current.as_mut() {
                            entry.href = read_text_until(&mut reader, "href")?;
                        }
                    }
                    "displayname" => {
                        if let Some(entry) = current.as_mut() {
                            entry.displayname = Some(read_text_until(&mut reader, "displayname")?);
                        }
                    }
                    "getcontentlength" => {
                        if let Some(entry) = current.as_mut() {
                            let raw = read_text_until(&mut reader, "getcontentlength")?;
                            entry.size = raw.trim().parse::<u64>().ok();
                        }
                    }
                    "getcontenttype" => {
                        if let Some(entry) = current.as_mut() {
                            let raw = read_text_until(&mut reader, "getcontenttype")?;
                            let trimmed = raw.trim();
                            if !trimmed.is_empty() {
                                entry.content_type = Some(trimmed.to_string());
                            }
                        }
                    }
                    "getlastmodified" => {
                        if let Some(entry) = current.as_mut() {
                            let raw = read_text_until(&mut reader, "getlastmodified")?;
                            // RFC 1123 / HTTP-date, e.g.
                            // "Tue, 21 Apr 2026 10:00:00 GMT"
                            entry.modified = DateTime::parse_from_rfc2822(raw.trim())
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc));
                        }
                    }
                    "resourcetype" => {
                        // If the subtree contains <collection/>, this is a
                        // folder. We walk the subtree event-by-event rather
                        // than skipping it, so we can inspect children.
                        if let Some(entry) = current.as_mut() {
                            entry.is_dir = resourcetype_is_collection(&mut reader)?;
                        }
                    }
                    _ => {}
                }
            }
            Event::Empty(e)
                // Empty-tag form of the same elements. `<d:collection/>`
                // only matters *inside* resourcetype, which we handle via
                // the Start branch above.
                if local_name(&e) == "resourcetype" => {
                    // Empty resourcetype = not a collection.
                    if let Some(entry) = current.as_mut() {
                        entry.is_dir = false;
                    }
                }
            Event::End(e) => {
                if local_name_end(&e) == "response"
                    && let Some(partial) = current.take()
                    && let Some(entry) =
                        partial.into_entry(&user_prefix, &request_prefix_encoded)
                {
                    entries.push(entry);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(entries)
}

#[derive(Default)]
struct PartialEntry {
    href: String,
    displayname: Option<String>,
    is_dir: bool,
    size: Option<u64>,
    content_type: Option<String>,
    modified: Option<DateTime<Utc>>,
}

impl PartialEntry {
    /// Convert into a `FileEntry`, or `None` if this response describes
    /// the request target itself (folder being listed), which we skip.
    fn into_entry(self, user_prefix: &str, request_prefix_encoded: &str) -> Option<FileEntry> {
        // Nextcloud hrefs are absolute paths, e.g.
        // "/remote.php/dav/files/alice/Documents/report.pdf".
        // Trim them down to the portion under the user root.
        let href = self.href.trim();
        let under_user = href.strip_prefix(user_prefix).unwrap_or(href);
        // The request target echoes itself — skip it.
        if under_user == request_prefix_encoded
            || under_user.trim_end_matches('/') == request_prefix_encoded.trim_end_matches('/')
        {
            return None;
        }

        let decoded = decode_path(under_user);
        // Prefer displayname from the server; fall back to the last
        // segment of the decoded path. Strip trailing slash for folders
        // so the UI label is clean.
        let name = self
            .displayname
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                decoded
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string()
            });

        Some(FileEntry {
            name,
            path: decoded,
            is_dir: self.is_dir,
            size: if self.is_dir { None } else { self.size },
            content_type: if self.is_dir { None } else { self.content_type },
            modified: self.modified,
        })
    }
}

fn resourcetype_is_collection(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> Result<bool, quick_xml::Error> {
    use quick_xml::events::Event;
    let mut is_collection = false;
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) if local_name(&e) == "collection" => {
                is_collection = true;
            }
            Event::End(e) if local_name_end(&e) == "resourcetype" => {
                return Ok(is_collection);
            }
            Event::Eof => return Ok(is_collection),
            _ => {}
        }
    }
}

// ── Small shared xml helpers (lifted from unkai-carddav patterns) ─

fn local_name(start: &quick_xml::events::BytesStart<'_>) -> String {
    strip_prefix_lowercase(start.name().as_ref())
}

fn local_name_end(end: &quick_xml::events::BytesEnd<'_>) -> String {
    strip_prefix_lowercase(end.name().as_ref())
}

fn strip_prefix_lowercase(bytes: &[u8]) -> String {
    let local = match bytes.iter().position(|&b| b == b':') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    };
    String::from_utf8_lossy(local).to_ascii_lowercase()
}

fn read_text_until(
    reader: &mut quick_xml::Reader<&[u8]>,
    start_local: &str,
) -> Result<String, quick_xml::Error> {
    use quick_xml::events::Event;
    let mut buf = String::new();
    loop {
        match reader.read_event()? {
            Event::Text(t) => buf.push_str(&t.xml_content().unwrap_or_default()),
            Event::CData(c) => buf.push_str(&String::from_utf8_lossy(&c)),
            Event::End(e) if strip_prefix_lowercase(e.name().as_ref()) == start_local => {
                return Ok(buf);
            }
            Event::Eof => return Ok(buf),
            _ => {}
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_path_segments_but_keeps_slashes() {
        assert_eq!(
            encode_path("/Documents/Q1 report.pdf"),
            "/Documents/Q1%20report.pdf"
        );
        assert_eq!(encode_path("/"), "/");
        assert_eq!(encode_path(""), "");
        // Unicode: ä = 0xC3 0xA4
        assert_eq!(encode_path("/Björk.txt"), "/Bj%C3%B6rk.txt");
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(decode_path("Q1%20report.pdf"), "Q1 report.pdf");
        assert_eq!(decode_path("Bj%C3%B6rk.txt"), "Björk.txt");
        // Malformed escape — leave byte untouched rather than erroring.
        assert_eq!(decode_path("half%2"), "half%2");
    }

    #[test]
    fn normalises_input_path() {
        assert_eq!(normalise_input_path(""), "/");
        assert_eq!(normalise_input_path("/"), "/");
        assert_eq!(normalise_input_path("Documents"), "/Documents");
        assert_eq!(normalise_input_path("/Documents/"), "/Documents");
        assert_eq!(normalise_input_path("Documents/Work/"), "/Documents/Work");
    }

    /// Realistic Nextcloud 28 PROPFIND response for `/Documents/` with
    /// one subfolder and one file — enough to exercise the parser.
    const PROPFIND_SAMPLE: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/Documents/</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>Documents</d:displayname>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:getlastmodified>Tue, 21 Apr 2026 10:00:00 GMT</d:getlastmodified>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Documents/Work/</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>Work</d:displayname>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:getlastmodified>Mon, 20 Apr 2026 09:00:00 GMT</d:getlastmodified>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Documents/Q1%20report.pdf</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>Q1 report.pdf</d:displayname>
        <d:resourcetype/>
        <d:getcontentlength>12345</d:getcontentlength>
        <d:getcontenttype>application/pdf</d:getcontenttype>
        <d:getlastmodified>Sun, 19 Apr 2026 15:30:00 GMT</d:getlastmodified>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn parses_propfind_listing() {
        let entries = parse_multistatus(PROPFIND_SAMPLE, "alice", "/Documents/").unwrap();
        // Self-entry is filtered out.
        assert_eq!(entries.len(), 2);

        let folder = &entries[0];
        assert_eq!(folder.name, "Work");
        assert_eq!(folder.path, "/Documents/Work/");
        assert!(folder.is_dir);
        assert_eq!(folder.size, None);
        assert_eq!(folder.content_type, None);

        let file = &entries[1];
        assert_eq!(file.name, "Q1 report.pdf");
        assert_eq!(file.path, "/Documents/Q1 report.pdf");
        assert!(!file.is_dir);
        assert_eq!(file.size, Some(12345));
        assert_eq!(file.content_type.as_deref(), Some("application/pdf"));
        assert!(file.modified.is_some());
    }

    #[test]
    fn parses_root_listing_with_shouty_prefix() {
        // Some older servers emit uppercase DAV: prefix. Strip-and-lower
        // handling should keep this working.
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/remote.php/dav/files/alice/</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>alice</D:displayname>
        <D:resourcetype><D:collection/></D:resourcetype>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/remote.php/dav/files/alice/Readme.md</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>Readme.md</D:displayname>
        <D:resourcetype/>
        <D:getcontentlength>42</D:getcontentlength>
        <D:getcontenttype>text/markdown</D:getcontenttype>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

        let entries = parse_multistatus(xml, "alice", "/").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Readme.md");
        assert_eq!(entries[0].path, "/Readme.md");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, Some(42));
    }

    #[test]
    fn empty_directory_listing() {
        // Only the self-response, no children.
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/remote.php/dav/files/alice/Empty/</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>Empty</d:displayname>
        <d:resourcetype><d:collection/></d:resourcetype>
      </d:prop>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let entries = parse_multistatus(xml, "alice", "/Empty/").unwrap();
        assert!(entries.is_empty());
    }
}

//! Nextcloud public share links via the OCS Files Sharing API.
//!
//! # Why a separate module from `files`
//!
//! `files.rs` speaks **WebDAV** — a low-level resource protocol on
//! `/remote.php/dav/...`. Sharing speaks **OCS** — a higher-level JSON
//! API on `/ocs/v2.php/apps/files_sharing/...`. Different endpoint,
//! different content type, different response envelope. Keeping them
//! apart means each module's auth/error/parsing pattern stays small
//! and the next person finding "the share code" doesn't have to skim
//! a 600-line WebDAV file.
//!
//! # Endpoint shape
//!
//! ```text
//!   POST {server}/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json
//!   OCS-APIRequest: true
//!   Accept: application/json
//!   Content-Type: application/x-www-form-urlencoded
//!
//!   path=/Documents/foo.pdf
//!   shareType=3            # 3 = public link
//!   permissions=1          # 1 = read-only (default for public links)
//! ```
//!
//! The response is the standard OCS envelope:
//!
//! ```json
//! {
//!   "ocs": {
//!     "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
//!     "data": {
//!       "id": "42",
//!       "url": "https://cloud.example.com/s/abc123",
//!       "token": "abc123",
//!       ...
//!     }
//!   }
//! }
//! ```
//!
//! The `url` field is what we hand back to the UI to paste into an
//! email body.
//!
//! # MVP scope
//!
//! For Phase 2 of issue #12 we create the simplest possible share —
//! read-only, no password, no expiry. Password / expiry / per-share
//! permissions can each be added as form fields later without breaking
//! the function signature; we'd just expand `ShareOptions` and pass it
//! through.

use serde::Deserialize;

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;

use crate::client;

/// Nextcloud share-type discriminator. We only ever create type 3
/// (public link) here — user/group/team shares are a different feature
/// and a different UI gesture.
const SHARE_TYPE_PUBLIC_LINK: u8 = 3;

/// Nextcloud's permission bitfield for shares:
/// 1=read, 2=update, 4=create, 8=delete, 16=share.  The Tauri layer
/// passes the chosen value straight through so the UI can mirror
/// Nextcloud's own "View only / Allow editing / …" picker.
pub const PERM_READ_ONLY: u8 = 1;

/// What the caller gets back after creating a share.  The `id` is
/// stored so callers can later update / delete the share without
/// re-fetching it from the server (e.g. patching the label as the
/// user edits the recipient list mid-compose, #91 follow-up).
#[derive(Debug, Clone)]
pub struct PublicShare {
    /// Stable Nextcloud share id (string-encoded integer).
    pub id: String,
    /// Public URL the recipient opens, e.g. `https://cloud.example.com/s/abc123`.
    pub url: String,
}

// ── Wire format ────────────────────────────────────────────────
//
// We can't use a single `OcsEnvelope<ShareData>` like capabilities.rs
// does, because on failure Nextcloud sends `"data": []` (an array, not
// the expected object) — strict serde fails on the data field before
// we ever get to inspect meta. So we deserialize meta first, then
// conditionally pull data into the right shape.

#[derive(Debug, Deserialize)]
struct OcsRaw {
    ocs: OcsBodyRaw,
}

#[derive(Debug, Deserialize)]
struct OcsBodyRaw {
    meta: OcsMeta,
    /// Held as opaque JSON until we know meta said "ok"; then we
    /// re-deserialize into the concrete payload type.
    #[serde(default)]
    data: serde_json::Value,
}

/// `statuscode` is the OCS-level status (separate from HTTP status).
/// On a successful share, `status == "ok"` and `statuscode == 200`.
/// On a denied share (e.g. sharing disabled by admin) Nextcloud may
/// still return HTTP 200 but `statuscode == 403` — so we have to
/// inspect this even after a 2xx HTTP response.
#[derive(Debug, Deserialize)]
struct OcsMeta {
    status: String,
    statuscode: u16,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShareData {
    /// Nextcloud serializes the id as a string ("42") in modern
    /// versions and as a number in some older releases — accept both
    /// via `serde_json::Value` and `to_string()` it.
    id: serde_json::Value,
    url: String,
}

/// Create a public share link for a file in the user's Nextcloud.
///
/// `path` is the same `/Documents/foo.pdf`-shaped path the file picker
/// produces. Returns the public URL on success.
///
/// # Errors
/// - `UnkaiError::Auth` — app password rejected (401).
/// - `UnkaiError::Nextcloud` — non-2xx HTTP, or OCS-level failure
///   (e.g. sharing globally disabled, target not found, file not in
///   user's scope). The OCS message is included where available so
///   the UI can show something specific.
/// - `UnkaiError::Protocol` — JSON didn't match the expected shape.
pub async fn create_public_share(
    server_url: &str,
    username: &str,
    app_password: &str,
    path: &str,
    password: Option<&str>,
    label: Option<&str>,
    permissions: u8,
    expire_date: Option<&str>,
    trusted_certs: &[TrustedCert],
) -> Result<PublicShare, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json");

    tracing::debug!(
        "POST {url} for path {path} (password: {}, label: {}, permissions: {}, expiry: {})",
        if password.is_some() { "yes" } else { "no" },
        if label.is_some() { "yes" } else { "no" },
        permissions,
        if expire_date.is_some() { "yes" } else { "no" }
    );

    let http = client::build(trusted_certs)?;
    // Build the form pairs dynamically — `password` and `label` are
    // only added when the caller actually supplied them.  Passing an
    // empty `password=` makes Nextcloud reject the request with
    // "Password too short" on some configurations; an empty `label=`
    // overwrites Nextcloud's auto-derived name with an empty string.
    // Omitting either field entirely is safer than sending empty.
    let share_type = SHARE_TYPE_PUBLIC_LINK.to_string();
    let permissions_s = permissions.to_string();
    let mut form: Vec<(&str, &str)> = vec![
        ("path", path),
        ("shareType", &share_type),
        ("permissions", &permissions_s),
    ];
    if let Some(pw) = password
        && !pw.is_empty()
    {
        form.push(("password", pw));
    }
    if let Some(lbl) = label
        && !lbl.is_empty()
    {
        form.push(("label", lbl));
    }
    // `expireDate` accepts `YYYY-MM-DD` -- the same format our
    // `DateField` component emits.  Once the date passes the
    // recipient sees a "Link expired" page instead of the file
    // contents.  Server-side default-expiration policies (admin
    // configured) may still clamp this down to a shorter window;
    // the OCS response surfaces that case via `meta.message`.
    if let Some(exp) = expire_date
        && !exp.is_empty()
    {
        form.push(("expireDate", exp));
    }

    let resp = http
        .post(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        // The form encoder URL-encodes for us, so we pass the raw path
        // (with spaces / unicode) and Nextcloud receives the right thing.
        .form(&form)
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("share request failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }

    // Read the body up front (success or failure) so a 4xx still
    // surfaces Nextcloud's actual reason. Password-policy rejections
    // come back as HTTP 400 with an OCS envelope whose `meta.message`
    // says e.g. "Password is too short" — pulling that into the
    // error makes the bad-password case actionable instead of "share
    // returned HTTP 400".
    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("share body read failed: {e}")))?;

    if !status.is_success() {
        let detail = ocs_message(&body)
            .map(|m| friendly_share_error(&m))
            .unwrap_or_else(|| {
                // Truncate so a verbose HTML error page doesn't blow
                // up the toast — 240 chars is enough to expose the
                // gist.
                let trimmed = body.trim();
                if trimmed.len() > 240 {
                    format!("{}…", &trimmed[..240])
                } else {
                    trimmed.to_string()
                }
            });
        return Err(UnkaiError::Nextcloud(detail));
    }

    parse_share_response(&body)
}

/// Try to lift the human-readable `meta.message` out of an OCS
/// response body. Returns `None` if the body isn't OCS-shaped JSON
/// or doesn't carry a message — caller falls back to the raw body
/// in that case.
fn ocs_message(body: &str) -> Option<String> {
    let raw: OcsRaw = serde_json::from_str(body).ok()?;
    raw.ocs.meta.message.filter(|m| !m.is_empty())
}

/// Map well-known Nextcloud password-policy / share-creation
/// messages to phrasings the user actually understands. The OCS
/// strings themselves are technically correct ("Password is among
/// the 1,000,000 most common passwords", "Password needs to be at
/// least 10 characters long.") but they read like server
/// diagnostics — surface them as guidance instead. Anything we
/// don't recognise falls through verbatim so we never hide the
/// real reason.
fn friendly_share_error(raw: &str) -> String {
    let lower = raw.to_lowercase();

    // Length policy. NC's wording varies between minor versions
    // ("Password needs to be at least N characters long.",
    // "Password is too short", "The password is too short.") so we
    // pull the digits when we can and fall back to a generic floor.
    if lower.contains("password") && (lower.contains("short") || lower.contains("at least")) {
        if let Some(min_len) = first_number(raw) {
            return format!("Password is too short. Choose at least {min_len} characters.");
        }
        return "Password is too short. Try a longer one.".to_string();
    }

    // Common-password blocklist — the policy app rejects the top-N
    // breach list.
    if lower.contains("most common passwords") || lower.contains("commonly used password") {
        return "That password is on a public list of common passwords. Pick something less guessable.".to_string();
    }

    // Numeric / character-class requirements.
    if lower.contains("password") && lower.contains("numeric") {
        return "Password needs at least one number.".to_string();
    }
    if lower.contains("password")
        && (lower.contains("special character") || lower.contains("special-character"))
    {
        return "Password needs at least one special character.".to_string();
    }
    if lower.contains("password") && lower.contains("upper") {
        return "Password needs at least one uppercase letter.".to_string();
    }
    if lower.contains("password") && lower.contains("lower") {
        return "Password needs at least one lowercase letter.".to_string();
    }

    // Fallback — keep the server's text but capitalise + add a final
    // period so it reads like a sentence rather than a log line.
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "The Nextcloud server rejected the share.".to_string();
    }
    let mut out = trimmed.to_string();
    if !out.ends_with('.') && !out.ends_with('!') && !out.ends_with('?') {
        out.push('.');
    }
    out
}

/// Pull the first run of ASCII digits out of a string and parse as
/// `u32`. Used to recover the "at least N" minimum length from the
/// password-policy app's message regardless of phrasing.
fn first_number(s: &str) -> Option<u32> {
    let mut digits = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if !digits.is_empty() {
            break;
        }
    }
    digits.parse().ok()
}

/// Parse the OCS envelope and surface either the URL or a meaningful
/// error. Split out so tests can drive it with canned JSON.
fn parse_share_response(body: &str) -> Result<PublicShare, UnkaiError> {
    let raw: OcsRaw = serde_json::from_str(body)
        .map_err(|e| UnkaiError::Protocol(format!("share bad JSON: {e}")))?;

    // OCS-level failure even though HTTP was 2xx — surface the server's
    // message so the user sees "Sharing is disabled" rather than a
    // generic error. Check meta first; on failure `data` is an empty
    // array and would never deserialize into ShareData.
    if raw.ocs.meta.status != "ok" || raw.ocs.meta.statuscode >= 400 {
        let msg = raw
            .ocs
            .meta
            .message
            .unwrap_or_else(|| "share rejected by server".to_string());
        return Err(UnkaiError::Nextcloud(format!(
            "share failed (OCS {}): {}",
            raw.ocs.meta.statuscode, msg
        )));
    }

    let data: ShareData = serde_json::from_value(raw.ocs.data)
        .map_err(|e| UnkaiError::Protocol(format!("share data bad shape: {e}")))?;
    let id = match data.id {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        other => {
            return Err(UnkaiError::Protocol(format!("share id bad shape: {other}")));
        }
    };
    Ok(PublicShare { id, url: data.url })
}

/// Update an existing public share's label (#91 follow-up).
///
/// Lets Compose track shares it minted earlier in a draft and
/// re-PUT a fresh `For: <recipients>` label whenever the user
/// edits the To / Cc / Bcc fields after the link was already
/// dropped into the body.  Without this, the Nextcloud audit
/// trail freezes whatever the recipient string was at click time.
///
/// Endpoint:
/// ```text
///   PUT  /ocs/v2.php/apps/files_sharing/api/v1/shares/{id}?format=json
///   OCS-APIRequest: true
///   label=<new label>
/// ```
///
/// An empty `label` would clobber the existing one with the empty
/// string on Nextcloud's side; callers should skip the call when
/// they have nothing to write rather than relying on an empty-
/// string short-circuit here.
pub async fn update_share_label(
    server_url: &str,
    username: &str,
    app_password: &str,
    share_id: &str,
    label: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    if share_id.is_empty() {
        return Err(UnkaiError::Other("share_id is empty".into()));
    }
    let server = client::normalize_server_url(server_url);
    let url =
        format!("{server}/ocs/v2.php/apps/files_sharing/api/v1/shares/{share_id}?format=json");

    tracing::debug!("PUT {url} for share {share_id} (label len {})", label.len());

    let http = client::build(trusted_certs)?;
    let resp = http
        .put(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .form(&[("label", label)])
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("share label update failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("share label body read failed: {e}")))?;

    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(
            ocs_message(&body)
                .map(|m| friendly_share_error(&m))
                .unwrap_or_else(|| body.trim().to_string()),
        ));
    }

    // Same OCS-success-but-failure check as create_public_share.
    if let Ok(raw) = serde_json::from_str::<OcsRaw>(&body)
        && (raw.ocs.meta.status != "ok" || raw.ocs.meta.statuscode >= 400)
    {
        return Err(UnkaiError::Nextcloud(format!(
            "share label update failed (OCS {}): {}",
            raw.ocs.meta.statuscode,
            raw.ocs.meta.message.unwrap_or_default()
        )));
    }
    Ok(())
}

/// Delete an existing public share by id (#193).
///
/// Used by the Compose flow when the user cancels a draft after
/// having minted share links via the Nextcloud file picker —
/// otherwise those shares dangle in the user's "Shared with
/// others" list with no associated mail.
///
/// Endpoint:
/// ```text
///   DELETE  /ocs/v2.php/apps/files_sharing/api/v1/shares/{id}?format=json
///   OCS-APIRequest: true
/// ```
///
/// The OCS DELETE returns 200 + `meta.statuscode = 200` on
/// success.  A `meta.statuscode = 404` (share already gone — race
/// with another deleter, manual cleanup, etc.) is treated as a
/// non-error: the caller wanted the share gone and it is.
pub async fn delete_share(
    server_url: &str,
    username: &str,
    app_password: &str,
    share_id: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    if share_id.is_empty() {
        return Err(UnkaiError::Other("share_id is empty".into()));
    }
    let server = client::normalize_server_url(server_url);
    let url =
        format!("{server}/ocs/v2.php/apps/files_sharing/api/v1/shares/{share_id}?format=json");

    tracing::debug!("DELETE {url} for share {share_id}");

    let http = client::build(trusted_certs)?;
    let resp = http
        .delete(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("share delete failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("share delete body read failed: {e}")))?;

    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(
            ocs_message(&body)
                .map(|m| friendly_share_error(&m))
                .unwrap_or_else(|| body.trim().to_string()),
        ));
    }

    if let Ok(raw) = serde_json::from_str::<OcsRaw>(&body)
        && raw.ocs.meta.status != "ok"
        && raw.ocs.meta.statuscode != 404
    {
        return Err(UnkaiError::Nextcloud(format!(
            "share delete failed (OCS {}): {}",
            raw.ocs.meta.statuscode,
            raw.ocs.meta.message.unwrap_or_default()
        )));
    }
    Ok(())
}

/// Snapshot of a single public share returned by `list_public_shares`
/// (#117).  Mirrors the OCS `shares` GET response with the fields the
/// share-management UI actually surfaces — `id`, what file/folder is
/// being shared, the public URL, optional label/password/expiry/
/// permission metadata.
///
/// Password is reported as a boolean only: the OCS response carries
/// either `null` (no password) or a bcrypt-style hash, and exposing
/// the hash to the frontend would be misleading.  The UI re-PUTs a
/// fresh password via `update_public_share` when the user changes it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicShareInfo {
    pub id: String,
    /// Path in the user's Nextcloud root (e.g. `/Documents/foo.pdf`).
    pub path: String,
    /// "file" or "folder" — drives which permission options the UI
    /// can offer when editing the share.
    pub item_type: String,
    /// Public URL the recipient opens.
    pub url: String,
    /// Bare token (the trailing `/s/<token>` segment); handy for
    /// cross-referencing with `data-unkai-share-id` markers in
    /// draft bodies even if the share id round-trips elsewhere.
    pub token: String,
    /// Human-readable label set on the share (#91).  `None` when the
    /// user never set one.
    pub label: Option<String>,
    /// Nextcloud permission bitmask (1 read, 2 update, 4 create,
    /// 8 delete, 16 share).
    pub permissions: u8,
    /// True when the share is gated behind a password.  The actual
    /// password is never sent to the client.
    pub has_password: bool,
    /// Expiration date in `YYYY-MM-DD` form, or `None` when the
    /// share never expires.  Nextcloud reports expiry as
    /// `YYYY-MM-DD HH:MM:SS` so we trim the time portion.
    pub expiration: Option<String>,
    /// Unix timestamp (seconds) of when the share was created.
    pub stime: i64,
    /// File mimetype as reported by Nextcloud; helps the UI pick the
    /// right icon glyph.  May be empty for folders.
    pub mimetype: String,
}

#[derive(Debug, Deserialize)]
struct ListShareItem {
    id: serde_json::Value,
    #[serde(default)]
    share_type: i64,
    #[serde(default)]
    path: String,
    #[serde(default)]
    item_type: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    permissions: u8,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    expiration: Option<String>,
    #[serde(default)]
    stime: i64,
    #[serde(default)]
    mimetype: String,
}

/// List every public share link the authenticated user currently owns
/// (#117).
///
/// Drives the share-management UI: the user wants to see all shares
/// they've created across every file so they can revoke, re-password,
/// or repermission them in one place.
///
/// Endpoint:
/// ```text
///   GET  /ocs/v2.php/apps/files_sharing/api/v1/shares?format=json
///   OCS-APIRequest: true
/// ```
///
/// We filter to `share_type == 3` (public link) on the client side —
/// the OCS endpoint returns user / group / circle shares mixed in,
/// but the management UI is scoped to link shares since those are the
/// ones where password / expiry / permission tweaks make sense.
pub async fn list_public_shares(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<PublicShareInfo>, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json");

    tracing::debug!("GET {url}");

    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("share list failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("share list body read failed: {e}")))?;
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(
            ocs_message(&body)
                .map(|m| friendly_share_error(&m))
                .unwrap_or_else(|| body.trim().to_string()),
        ));
    }
    parse_list_response(&body)
}

fn parse_list_response(body: &str) -> Result<Vec<PublicShareInfo>, UnkaiError> {
    let raw: OcsRaw = serde_json::from_str(body)
        .map_err(|e| UnkaiError::Protocol(format!("share list bad JSON: {e}")))?;
    if raw.ocs.meta.status != "ok" || raw.ocs.meta.statuscode >= 400 {
        let msg = raw
            .ocs
            .meta
            .message
            .unwrap_or_else(|| "share list rejected by server".to_string());
        return Err(UnkaiError::Nextcloud(format!(
            "share list failed (OCS {}): {}",
            raw.ocs.meta.statuscode, msg
        )));
    }
    let items: Vec<ListShareItem> = serde_json::from_value(raw.ocs.data)
        .map_err(|e| UnkaiError::Protocol(format!("share list data bad shape: {e}")))?;
    let mut out: Vec<PublicShareInfo> = Vec::with_capacity(items.len());
    for it in items {
        // Filter to public-link shares only — user / group / circle
        // shares ride along on the same endpoint but live in their
        // own UI surface.
        if it.share_type != SHARE_TYPE_PUBLIC_LINK as i64 {
            continue;
        }
        let id = match it.id {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            _ => continue,
        };
        // Trim Nextcloud's "YYYY-MM-DD HH:MM:SS" expiry down to the
        // date — the DateField on the UI side round-trips through
        // `YYYY-MM-DD`, so passing the full timestamp back would
        // refuse to load into the picker.
        let expiration = it.expiration.and_then(|s| {
            let trimmed = s.split_whitespace().next().unwrap_or("").to_string();
            if trimmed.is_empty() || trimmed == "null" {
                None
            } else {
                Some(trimmed)
            }
        });
        out.push(PublicShareInfo {
            id,
            path: it.path,
            item_type: it.item_type,
            url: it.url,
            token: it.token,
            label: it.label.filter(|s| !s.is_empty()),
            permissions: it.permissions,
            has_password: it
                .password
                .as_deref()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            expiration,
            stime: it.stime,
            mimetype: it.mimetype,
        });
    }
    Ok(out)
}

/// Update the mutable properties of an existing public share (#117).
///
/// Endpoint:
/// ```text
///   PUT  /ocs/v2.php/apps/files_sharing/api/v1/shares/{id}?format=json
///   OCS-APIRequest: true
/// ```
///
/// Each field is optional — only the ones the caller passes get sent
/// to the server, mirroring how the OCS endpoint treats absent form
/// fields as "leave alone".  Two sentinels let the UI explicitly
/// clear a value rather than just skip it:
///   - `password = Some("")` removes any existing password.
///   - `expire_date = Some("")` removes the expiration date.
///
/// Permission updates pass the raw bitmask Nextcloud uses
/// (1 read, 2 update, 4 create, 8 delete, 16 share).
pub async fn update_public_share(
    server_url: &str,
    username: &str,
    app_password: &str,
    share_id: &str,
    password: Option<&str>,
    permissions: Option<u8>,
    expire_date: Option<&str>,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    if share_id.is_empty() {
        return Err(UnkaiError::Other("share_id is empty".into()));
    }
    if password.is_none() && permissions.is_none() && expire_date.is_none() {
        // Nothing to update — short-circuit so we don't burn a round
        // trip on an empty PUT.
        return Ok(());
    }
    let server = client::normalize_server_url(server_url);
    let url =
        format!("{server}/ocs/v2.php/apps/files_sharing/api/v1/shares/{share_id}?format=json");

    tracing::debug!(
        "PUT {url} (password: {}, permissions: {}, expiry: {})",
        if password.is_some() { "yes" } else { "no" },
        permissions
            .map(|p| p.to_string())
            .unwrap_or_else(|| "n/a".to_string()),
        if expire_date.is_some() { "yes" } else { "no" }
    );

    let permissions_s = permissions.map(|p| p.to_string());
    let mut form: Vec<(&str, &str)> = Vec::new();
    if let Some(pw) = password {
        // Empty string is the "clear password" sentinel — Nextcloud
        // accepts `password=` (no value) to remove the gate.
        form.push(("password", pw));
    }
    if let Some(ref s) = permissions_s {
        form.push(("permissions", s));
    }
    if let Some(exp) = expire_date {
        // Empty string is the "clear expiry" sentinel — Nextcloud
        // removes the expiration date when the form value is empty.
        form.push(("expireDate", exp));
    }

    let http = client::build(trusted_certs)?;
    let resp = http
        .put(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .form(&form)
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("share update failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| UnkaiError::Network(format!("share update body read failed: {e}")))?;
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(
            ocs_message(&body)
                .map(|m| friendly_share_error(&m))
                .unwrap_or_else(|| body.trim().to_string()),
        ));
    }
    if let Ok(raw) = serde_json::from_str::<OcsRaw>(&body)
        && (raw.ocs.meta.status != "ok" || raw.ocs.meta.statuscode >= 400)
    {
        return Err(UnkaiError::Nextcloud(format!(
            "share update failed (OCS {}): {}",
            raw.ocs.meta.statuscode,
            raw.ocs.meta.message.unwrap_or_default()
        )));
    }
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal slice of a real Nextcloud 28 share response. The actual
    /// `data` object has 30+ fields; we only care about `url`.
    const OK_RESPONSE: &str = r#"{
      "ocs": {
        "meta": {
          "status": "ok",
          "statuscode": 200,
          "message": "OK"
        },
        "data": {
          "id": "42",
          "url": "https://cloud.example.com/s/abc123",
          "token": "abc123",
          "share_type": 3,
          "permissions": 1
        }
      }
    }"#;

    #[test]
    fn parses_successful_share() {
        let share = parse_share_response(OK_RESPONSE).unwrap();
        assert_eq!(share.url, "https://cloud.example.com/s/abc123");
        assert_eq!(share.id, "42");
    }

    #[test]
    fn parses_share_id_from_number() {
        // Older Nextclouds returned the id as a JSON number.
        let body = r#"{
          "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": { "id": 99, "url": "https://cloud.example.com/s/x" }
          }
        }"#;
        let share = parse_share_response(body).unwrap();
        assert_eq!(share.id, "99");
    }

    /// Sharing globally disabled — Nextcloud returns HTTP 200 but
    /// `statuscode: 403`. We must surface that as a Nextcloud error so
    /// the user sees something actionable.
    #[test]
    fn surfaces_ocs_level_failure() {
        let body = r#"{
          "ocs": {
            "meta": {
              "status": "failure",
              "statuscode": 403,
              "message": "Public upload disabled by the administrator"
            },
            "data": []
          }
        }"#;
        let err = parse_share_response(body).unwrap_err();
        match err {
            UnkaiError::Nextcloud(msg) => {
                assert!(msg.contains("403"));
                assert!(msg.contains("Public upload disabled"));
            }
            other => panic!("expected Nextcloud error, got {other:?}"),
        }
    }

    /// Malformed JSON — should land in Protocol, not Network/Nextcloud.
    #[test]
    fn surfaces_bad_json_as_protocol_error() {
        let err = parse_share_response("not json at all").unwrap_err();
        assert!(matches!(err, UnkaiError::Protocol(_)));
    }

    /// `list_public_shares` parser: filters to share_type=3 and
    /// normalises optional fields.  Trims the `HH:MM:SS` portion off
    /// the expiry so the date round-trips through DateField.
    #[test]
    fn parses_list_response_filters_public_links() {
        let body = r#"{
          "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": [
              {
                "id": "1",
                "share_type": 3,
                "path": "/Documents/foo.pdf",
                "item_type": "file",
                "url": "https://cloud.example.com/s/abc123",
                "token": "abc123",
                "label": "for Alex",
                "permissions": 1,
                "password": "$2y$10$hash",
                "expiration": "2026-06-01 00:00:00",
                "stime": 1716700000,
                "mimetype": "application/pdf"
              },
              {
                "id": 2,
                "share_type": 0,
                "path": "/Documents/bar.txt",
                "item_type": "file",
                "url": "",
                "token": "",
                "label": null,
                "permissions": 1,
                "password": null,
                "expiration": null,
                "stime": 1716700100,
                "mimetype": "text/plain"
              },
              {
                "id": "3",
                "share_type": 3,
                "path": "/Shared",
                "item_type": "folder",
                "url": "https://cloud.example.com/s/def456",
                "token": "def456",
                "permissions": 15,
                "password": null,
                "expiration": null,
                "stime": 1716700200,
                "mimetype": "httpd/unix-directory"
              }
            ]
          }
        }"#;
        let out = parse_list_response(body).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "1");
        assert_eq!(out[0].path, "/Documents/foo.pdf");
        assert!(out[0].has_password);
        assert_eq!(out[0].expiration.as_deref(), Some("2026-06-01"));
        assert_eq!(out[0].label.as_deref(), Some("for Alex"));
        assert_eq!(out[1].id, "3");
        assert_eq!(out[1].item_type, "folder");
        assert!(!out[1].has_password);
        assert!(out[1].expiration.is_none());
    }
}

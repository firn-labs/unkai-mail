//! Two-way CalDAV: PUT (create / update) and DELETE for VEVENTs.
//!
//! Mirrors `nimbus-carddav::write` with `text/calendar` bodies.
//!
//! # Concurrency model
//!
//! - **Create**: PUT with `If-None-Match: *` so the server refuses to
//!   overwrite an existing UID at our chosen href. Pairs with our
//!   `{calendar_url}/{uid}.ics` href so two clients picking the same
//!   UID get a clean 412 instead of silently clobbering each other.
//! - **Update**: PUT with `If-Match: <etag>` â€” the server returns 412
//!   if the resource changed since our last sync. We surface that as a
//!   structured error so the caller can re-fetch and merge.
//! - **Delete**: same `If-Match` story.
//!
//! # Choosing the resource path
//!
//! For a fresh create, we pick `{calendar_url}/{uid}.ics`. Nextcloud
//! accepts that and returns the new etag in the response headers â€” no
//! follow-up PROPFIND required.

use reqwest::StatusCode;

use nimbus_core::NimbusError;
use nimbus_core::models::TrustedCert;

use crate::client::{build, delete_resource, normalize_server_url, put_ics};

/// Result of a successful create / update â€” the canonical href and
/// the new etag, both ready to drop into the local cache row.
#[derive(Debug, Clone)]
pub struct WriteOutcome {
    pub href: String,
    pub etag: String,
}

/// Create a new event in `calendar_url`. We pick the href as
/// `{calendar_url}/{uid}.ics` and PUT with `If-None-Match: *` so a
/// UID collision becomes a clean 412 instead of a silent overwrite.
/// The iCalendar body's `UID:` property must match `uid`.
pub async fn create_event(
    server_url: &str,
    calendar_url: &str,
    username: &str,
    app_password: &str,
    uid: &str,
    ics: &str,
    trusted_certs: &[TrustedCert],
) -> Result<WriteOutcome, NimbusError> {
    let http = build(trusted_certs)?;
    let href = build_href(calendar_url, uid);

    let resp = put_ics(&http, &href, username, app_password, ics, None, true).await?;
    let status = resp.status();
    if status == StatusCode::PRECONDITION_FAILED {
        return Err(NimbusError::Nextcloud(format!(
            "event with UID {uid} already exists on the server"
        )));
    }
    // 403 / 404 on the create-side PUT is the same permission
    // signal as on update — flag it so the caller can mark the
    // calendar read-only locally instead of letting the user
    // attempt the same write again (#236 follow-up).
    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Err(NimbusError::CalDavWriteForbidden(format!(
            "PUT new event {href} returned HTTP {status}"
        )));
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "PUT new event returned HTTP {status}"
        )));
    }

    let etag = read_etag(&resp).unwrap_or_default();
    Ok(WriteOutcome {
        href: absolute_or_passthrough(server_url, &href),
        etag,
    })
}

/// Update an existing event at `href`, gated on `if_match_etag`.
///
/// `href` should be the absolute href we cached when the event was
/// first synced. Returns the new etag the server assigned after our
/// PUT â€” the caller persists it so the next update keeps the
/// optimistic-concurrency chain unbroken.
pub async fn update_event(
    href: &str,
    username: &str,
    app_password: &str,
    if_match_etag: &str,
    ics: &str,
    trusted_certs: &[TrustedCert],
) -> Result<WriteOutcome, NimbusError> {
    let http = build(trusted_certs)?;
    let resp = put_ics(
        &http,
        href,
        username,
        app_password,
        ics,
        Some(if_match_etag),
        false,
    )
    .await?;
    let status = resp.status();
    if status == StatusCode::PRECONDITION_FAILED {
        // Programmatically-detectable variant â€” callers (the
        // calendar-write Tauri commands, the RSVP path) catch
        // `EtagMismatch`, run a single-calendar sync to pull
        // the latest etag, and retry transparently.  The user
        // never sees a "refresh and try again" toast.
        return Err(NimbusError::EtagMismatch(format!(
            "If-Match failed for {href} (server etag != cached)"
        )));
    }
    // 403 Forbidden / 404 Not Found on a PUT to an existing
    // event href is the server saying "no write access"
    // (#236 follow-up).  Sabre/DAV — NC's CalDAV stack —
    // returns 404 instead of 403 for forbidden resources as
    // a permission-masking pattern; we treat both the same so
    // the caller can flip the calendar's `read_only` flag and
    // stop the EventEditor offering writes on it.
    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Err(NimbusError::CalDavWriteForbidden(format!(
            "PUT {href} returned HTTP {status}"
        )));
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "PUT event returned HTTP {status}"
        )));
    }
    let etag = read_etag(&resp).unwrap_or_default();
    Ok(WriteOutcome {
        href: href.to_string(),
        etag,
    })
}

/// Delete an event at `href`, gated on `if_match_etag`.
pub async fn delete_event(
    href: &str,
    username: &str,
    app_password: &str,
    if_match_etag: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), NimbusError> {
    delete_event_inner(
        href,
        username,
        app_password,
        if_match_etag,
        false,
        trusted_certs,
    )
    .await
}

/// `delete_event` variant that suppresses Sabre/DAV's
/// auto-iTIP via `Schedule-Reply: F`.  Used by the
/// "Remove from my calendar" flow for a meeting the
/// organiser already cancelled â€” without this header Sabre
/// would emit a spurious `METHOD:REPLY;PARTSTAT=DECLINED`
/// iMIP at the organiser when the attendee removes their
/// local copy.
pub async fn delete_event_silent(
    href: &str,
    username: &str,
    app_password: &str,
    if_match_etag: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), NimbusError> {
    delete_event_inner(
        href,
        username,
        app_password,
        if_match_etag,
        true,
        trusted_certs,
    )
    .await
}

async fn delete_event_inner(
    href: &str,
    username: &str,
    app_password: &str,
    if_match_etag: &str,
    suppress_itip: bool,
    trusted_certs: &[TrustedCert],
) -> Result<(), NimbusError> {
    let http = build(trusted_certs)?;
    let resp = if suppress_itip {
        crate::client::delete_resource_no_itip(
            &http,
            href,
            username,
            app_password,
            Some(if_match_etag),
        )
        .await?
    } else {
        delete_resource(&http, href, username, app_password, Some(if_match_etag)).await?
    };
    let status = resp.status();
    if status == StatusCode::PRECONDITION_FAILED {
        // Programmatically-detectable variant — same shape as
        // `update_event` so the Tauri layer can transparently
        // sync + retry instead of surfacing a "refresh and try
        // again" toast to the user.  See
        // `delete_event_with_etag_retry` in `src-tauri`.
        return Err(NimbusError::EtagMismatch(format!(
            "If-Match failed for DELETE {href} (server etag != cached)"
        )));
    }
    // 404 is fine â€” already gone is the state we wanted.
    if status == StatusCode::NOT_FOUND {
        return Ok(());
    }
    // 403 Forbidden on DELETE → calendar is read-only on the
    // server side (#236 follow-up).  Same permission-masking
    // signal the PUT path uses; the caller flips the local
    // `read_only` flag so the editor stops offering writes.
    if status == StatusCode::FORBIDDEN {
        return Err(NimbusError::CalDavWriteForbidden(format!(
            "DELETE {href} returned HTTP {status}"
        )));
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "DELETE event returned HTTP {status}"
        )));
    }
    Ok(())
}

fn build_href(calendar_url: &str, uid: &str) -> String {
    let base = calendar_url.trim_end_matches('/');
    let safe_uid = uid_to_filename(uid);
    format!("{base}/{safe_uid}.ics")
}

/// Sanitise a UID for use as a path segment. Real-world VEVENT UIDs
/// are usually URN/UUID-shaped; this is belt-and-braces for anything
/// weird (spaces, slashes) so we don't get a 400 back.
fn uid_to_filename(uid: &str) -> String {
    uid.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

fn read_etag(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string())
}

/// If `href` is already absolute, return it. Otherwise prepend the
/// server origin â€” same semantics as `client::absolute_url`.
fn absolute_or_passthrough(server_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{}{}", normalize_server_url(server_url), href)
    } else {
        format!("{}/{}", normalize_server_url(server_url), href)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_href_joins_safely() {
        assert_eq!(
            build_href("https://x/dav/cal/", "abc-123"),
            "https://x/dav/cal/abc-123.ics"
        );
        assert_eq!(
            build_href("https://x/dav/cal", "abc-123"),
            "https://x/dav/cal/abc-123.ics"
        );
    }

    #[test]
    fn uid_to_filename_strips_path_chars() {
        assert_eq!(uid_to_filename("a/b c"), "a_b_c");
        assert_eq!(uid_to_filename("urn:uuid:1234"), "urn_uuid_1234");
    }
}

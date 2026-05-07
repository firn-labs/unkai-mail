//! Two-way CardDAV: PUT (create / update) and DELETE.
//!
//! # Concurrency model
//!
//! We use HTTP preconditions (`If-Match`, `If-None-Match: *`) as the
//! spec intends:
//!
//! - **Create**: `If-None-Match: *` makes the PUT atomic â€” the server
//!   refuses to overwrite an existing UID with that name. Pairs with
//!   our generated href so two clients picking the same UID can't
//!   silently clobber each other.
//! - **Update**: `If-Match: <etag>` makes the PUT optimistic â€” the
//!   server returns 412 if the resource changed since our last sync.
//!   We surface that as a structured error so the caller can re-fetch
//!   and merge.
//! - **Delete**: same `If-Match` story.
//!
//! # Identifying the new resource
//!
//! For a create, we choose the resource path ourselves:
//! `{addressbook_url}/{uid}.vcf`. Nextcloud accepts this and returns
//! the new etag in the response headers; we don't need a follow-up
//! PROPFIND.

use reqwest::StatusCode;

use nimbus_core::NimbusError;
use nimbus_core::models::TrustedCert;

use crate::client::{build, delete_resource, normalize_server_url, put_vcard};

/// Result of a successful create / update â€” the canonical href and
/// the new etag, both ready to drop into the local cache row.
#[derive(Debug, Clone)]
pub struct WriteOutcome {
    pub href: String,
    pub etag: String,
}

/// Create a new contact in `addressbook_url`. We pick the href as
/// `{addressbook_url}/{uid}.vcf` and PUT with `If-None-Match: *`
/// so a UID collision becomes a clean 412 instead of a silent
/// overwrite. The vCard `UID:` property must match `uid`.
pub async fn create_contact(
    server_url: &str,
    addressbook_url: &str,
    username: &str,
    app_password: &str,
    uid: &str,
    vcard: &str,
    trusted_certs: &[TrustedCert],
) -> Result<WriteOutcome, NimbusError> {
    let http = build(trusted_certs)?;
    let href = build_href(addressbook_url, uid);

    let resp = put_vcard(&http, &href, username, app_password, vcard, None, true).await?;
    let status = resp.status();
    if status == StatusCode::PRECONDITION_FAILED {
        return Err(NimbusError::Nextcloud(format!(
            "contact with UID {uid} already exists on the server"
        )));
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "PUT new contact returned HTTP {status}"
        )));
    }

    let etag = read_etag(&resp).unwrap_or_default();
    Ok(WriteOutcome {
        href: absolute_or_passthrough(server_url, &href),
        etag,
    })
}

/// Update an existing contact at `href`, gated on `if_match_etag`.
///
/// `href` should be the absolute href we cached when the contact was
/// first synced. Returns the new etag the server assigned after our
/// PUT â€” the caller persists it so the next update keeps the
/// optimistic-concurrency chain unbroken.
pub async fn update_contact(
    href: &str,
    username: &str,
    app_password: &str,
    if_match_etag: &str,
    vcard: &str,
    trusted_certs: &[TrustedCert],
) -> Result<WriteOutcome, NimbusError> {
    let http = build(trusted_certs)?;
    let resp = put_vcard(
        &http,
        href,
        username,
        app_password,
        vcard,
        Some(if_match_etag),
        false,
    )
    .await?;
    let status = resp.status();
    if status == StatusCode::PRECONDITION_FAILED {
        return Err(NimbusError::Nextcloud(
            "contact was modified on the server since last sync â€” refresh and try again"
                .to_string(),
        ));
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "PUT contact returned HTTP {status}"
        )));
    }
    let etag = read_etag(&resp).unwrap_or_default();
    Ok(WriteOutcome {
        href: href.to_string(),
        etag,
    })
}

/// Delete a contact at `href`, gated on `if_match_etag`.
pub async fn delete_contact(
    href: &str,
    username: &str,
    app_password: &str,
    if_match_etag: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), NimbusError> {
    let http = build(trusted_certs)?;
    let resp = delete_resource(&http, href, username, app_password, Some(if_match_etag)).await?;
    let status = resp.status();
    if status == StatusCode::PRECONDITION_FAILED {
        return Err(NimbusError::Nextcloud(
            "contact was modified on the server since last sync â€” refresh and try again"
                .to_string(),
        ));
    }
    // 404 is fine â€” already gone is the state we wanted.
    if !status.is_success() && status != StatusCode::NOT_FOUND {
        return Err(NimbusError::Nextcloud(format!(
            "DELETE contact returned HTTP {status}"
        )));
    }
    Ok(())
}

fn build_href(addressbook_url: &str, uid: &str) -> String {
    // Slash-trimming both sides so we don't end up with `â€¦/contacts//uid.vcf`.
    let base = addressbook_url.trim_end_matches('/');
    let safe_uid = uid_to_filename(uid);
    format!("{base}/{safe_uid}.vcf")
}

/// Sanitise a UID for use as a path segment. Real-world vCard UIDs
/// are usually URN/UUID-shaped already; this is belt-and-braces for
/// anything weird (spaces, slashes) so we don't get a 400 back.
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
            build_href("https://x/dav/ab/", "abc-123"),
            "https://x/dav/ab/abc-123.vcf"
        );
        assert_eq!(
            build_href("https://x/dav/ab", "abc-123"),
            "https://x/dav/ab/abc-123.vcf"
        );
    }

    #[test]
    fn uid_to_filename_strips_path_chars() {
        assert_eq!(uid_to_filename("a/b c"), "a_b_c");
        assert_eq!(uid_to_filename("urn:uuid:1234"), "urn_uuid_1234");
    }
}

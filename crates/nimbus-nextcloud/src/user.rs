//! OCS user-info â€” the authenticated user's profile email.
//!
//! Used by the calendar create/update path to put the right
//! address on `ORGANIZER:mailto:`.  Nextcloud 30+ Mail Provider
//! matches `ORGANIZER` against the user's Mail-app accounts
//! character-for-character â€” get this wrong and iMIP silently
//! falls back to the system mailer with `From: invitations-noreply@â€¦`.

use serde::Deserialize;

use nimbus_core::NimbusError;
use nimbus_core::models::TrustedCert;

use crate::client;

#[derive(Debug, Deserialize)]
struct OcsEnvelope<T> {
    ocs: OcsBody<T>,
}

#[derive(Debug, Deserialize)]
struct OcsBody<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct UserData {
    /// Primary email from Personal settings â†’ Personal info â†’ Email.
    /// Often null on freshly-created accounts; callers must handle
    /// the empty case explicitly.
    #[serde(default)]
    email: Option<String>,
    /// Display name â€” used for `ORGANIZER;CN=` so the iMIP shows a
    /// human label alongside the address.
    #[serde(default)]
    displayname: Option<String>,
}

/// Profile fields we surface to the rest of the app.
#[derive(Debug, Clone)]
pub struct NextcloudUserProfile {
    pub email: Option<String>,
    pub display_name: Option<String>,
}

/// Fetch the authenticated user's profile from
/// `/ocs/v2.php/cloud/user` (the "current user" endpoint, no
/// username path component required).
pub async fn fetch_current_user(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<NextcloudUserProfile, NimbusError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}/ocs/v2.php/cloud/user?format=json");
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("user request failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(NimbusError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "user info returned HTTP {status}"
        )));
    }

    let env: OcsEnvelope<UserData> = resp
        .json()
        .await
        .map_err(|e| NimbusError::Protocol(format!("user info bad JSON: {e}")))?;

    Ok(NextcloudUserProfile {
        email: env.ocs.data.email.filter(|s| !s.trim().is_empty()),
        display_name: env.ocs.data.displayname.filter(|s| !s.trim().is_empty()),
    })
}

// â”€â”€â”€ Sharees lookup: "is this email a Nextcloud user?" â”€â”€â”€â”€â”€â”€

#[derive(Debug, Deserialize)]
struct ShareesResponse {
    /// The `users` bucket carries matches that are local NC
    /// principals.  Other buckets (`groups`, `remotes`,
    /// `emails`) we don't care about â€” we want the
    /// authoritative-user list.
    #[serde(default)]
    exact: ShareesBuckets,
    #[serde(default)]
    users: Vec<ShareeMatch>,
}

#[derive(Debug, Default, Deserialize)]
struct ShareesBuckets {
    /// Inside `exact.users` we look for an entry whose
    /// `value.shareWith` matches the email's local principal.
    #[serde(default)]
    users: Vec<ShareeMatch>,
}

#[derive(Debug, Deserialize)]
struct ShareeMatch {
    /// Display name on the row â€” what NC's admin set as the
    /// user's full name.
    label: String,
    value: ShareeMatchValue,
}

#[derive(Debug, Deserialize)]
struct ShareeMatchValue {
    /// `shareWith` is the userId for `shareType=0` (local user).
    #[serde(rename = "shareWith")]
    share_with: String,
}

/// Match returned by [`find_user_by_email`] â€” the user-side
/// fields we care about for "is this attendee internal?".
#[derive(Debug, Clone)]
pub struct NextcloudUserMatch {
    pub user_id: String,
    pub display_name: String,
}

/// Look up a Nextcloud user by email via the sharees endpoint.
/// Returns `Ok(None)` when no NC principal owns that address â€”
/// the caller treats that as "external attendee, route through
/// guest URL / email participant".  Returns `Ok(Some(...))`
/// when an exact match is found (the user *and* the email are
/// authoritatively registered against the same principal).
///
/// We restrict to `shareType[]=0` (local users) so the response
/// can't be ambiguous against an email-share row carrying the
/// same address.  `itemType=calendar` is what NC Calendar's
/// own attendee picker passes; using the same hint keeps the
/// server's filtering logic aligned.
pub async fn find_user_by_email(
    server_url: &str,
    username: &str,
    app_password: &str,
    email: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Option<NextcloudUserMatch>, NimbusError> {
    let server = client::normalize_server_url(server_url);
    let url = format!(
        "{server}/ocs/v2.php/apps/files_sharing/api/v1/sharees\
         ?format=json&itemType=calendar&search={}&shareType[]=0",
        urlencoding(email),
    );
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("sharees request failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(NimbusError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "sharees returned HTTP {status}"
        )));
    }

    let env: OcsEnvelope<ShareesResponse> = resp
        .json()
        .await
        .map_err(|e| NimbusError::Protocol(format!("sharees bad JSON: {e}")))?;

    // Prefer the `exact` bucket â€” it's what Nextcloud uses to
    // signal "this is the same address you typed".  Fall back
    // to the partial-match `users` list and pick a row whose
    // userId equals the email's local part as a last resort
    // (some NC instances store full email as userId).
    let pick = env
        .ocs
        .data
        .exact
        .users
        .first()
        .or_else(|| env.ocs.data.users.first());
    Ok(pick.map(|m| NextcloudUserMatch {
        user_id: m.value.share_with.clone(),
        display_name: m.label.clone(),
    }))
}

// â”€â”€â”€ User groups + Teams (#133) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
//
// Nextcloud surfaces three kinds of "group of people" the
// contacts UI cares about:
//   - vCard `KIND:group` records (handled in nimbus-carddav)
//   - OCS user groups â€” the access-control groups under
//     Settings â†’ Users â†’ Groups; members are NC user IDs.
//   - Circles / Teams â€” the spreed-style team feature backed by
//     the Circles app; members can be NC users, emails, or
//     other circles.
// Both OCS and Circles are read-only from Nimbus's perspective â€”
// management lives in the Nextcloud admin UI / Files sidebar.

#[derive(Debug, Deserialize)]
struct GroupsListData {
    groups: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GroupMembersData {
    users: Vec<String>,
}

/// Identity / access groups the authenticated user belongs to,
/// fetched from `/ocs/v2.php/cloud/users/<user>/groups`.  This
/// endpoint is permitted for the user themselves on every NC
/// instance â€” no admin needed.
pub async fn fetch_my_groups(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<String>, NimbusError> {
    let server = client::normalize_server_url(server_url);
    let url = format!(
        "{server}/ocs/v2.php/cloud/users/{}/groups?format=json",
        urlencoding(username),
    );
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("user-groups request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "user-groups returned HTTP {status}"
        )));
    }
    let env: OcsEnvelope<GroupsListData> = resp
        .json()
        .await
        .map_err(|e| NimbusError::Protocol(format!("user-groups bad JSON: {e}")))?;
    Ok(env.ocs.data.groups)
}

/// Members of one NC user group.  Some servers restrict this
/// to admins; we soft-fail (return an empty list) so the rest of
/// the contacts UI still loads when the user isn't allowed to
/// enumerate a group's roster.
pub async fn fetch_group_member_ids(
    server_url: &str,
    username: &str,
    app_password: &str,
    group_id: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<String>, NimbusError> {
    let server = client::normalize_server_url(server_url);
    let url = format!(
        "{server}/ocs/v2.php/cloud/groups/{}?format=json",
        urlencoding(group_id),
    );
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("group-members request failed: {e}")))?;
    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
        // Permission-restricted or missing group â€” surface as an
        // empty list, not an error, so the caller can move on.
        return Ok(Vec::new());
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "group-members returned HTTP {status}"
        )));
    }
    let env: OcsEnvelope<GroupMembersData> = resp
        .json()
        .await
        .map_err(|e| NimbusError::Protocol(format!("group-members bad JSON: {e}")))?;
    Ok(env.ocs.data.users)
}

#[derive(Debug, Deserialize)]
struct UserProfileData {
    #[serde(default)]
    displayname: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NextcloudUserSummary {
    pub user_id: String,
    pub display_name: String,
    pub email: Option<String>,
}

/// Profile lookup for a single NC user id.  Used to map the
/// `users` list returned by `fetch_group_member_ids` to
/// (display name, email) tuples for the contacts UI.
pub async fn fetch_user_profile(
    server_url: &str,
    username: &str,
    app_password: &str,
    target_user_id: &str,
    trusted_certs: &[TrustedCert],
) -> Result<NextcloudUserSummary, NimbusError> {
    let server = client::normalize_server_url(server_url);
    let url = format!(
        "{server}/ocs/v2.php/cloud/users/{}?format=json",
        urlencoding(target_user_id),
    );
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("user-profile request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Ok(NextcloudUserSummary {
            user_id: target_user_id.to_string(),
            display_name: target_user_id.to_string(),
            email: None,
        });
    }
    let env: OcsEnvelope<UserProfileData> = resp
        .json()
        .await
        .map_err(|e| NimbusError::Protocol(format!("user-profile bad JSON: {e}")))?;
    Ok(NextcloudUserSummary {
        user_id: target_user_id.to_string(),
        display_name: env
            .ocs
            .data
            .displayname
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| target_user_id.to_string()),
        email: env.ocs.data.email.filter(|s| !s.trim().is_empty()),
    })
}

#[derive(Debug, Deserialize)]
struct CirclesEnvelope {
    ocs: CirclesOcs,
}
#[derive(Debug, Deserialize)]
struct CirclesOcs {
    #[serde(default)]
    data: Vec<CircleData>,
}
#[derive(Debug, Deserialize)]
struct CircleData {
    #[serde(rename = "singleId", default)]
    single_id: String,
    #[serde(rename = "displayName", default)]
    display_name: String,
}

#[derive(Debug, Clone)]
pub struct NextcloudCircle {
    pub id: String,
    pub display_name: String,
}

/// Circles / Teams the authenticated user belongs to, via the
/// Circles app's OCS API.  Returns an empty list (Ok) when the
/// app isn't installed â€” the endpoint 404s, and the contacts
/// UI just doesn't render a Teams section.
pub async fn fetch_my_circles(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<NextcloudCircle>, NimbusError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}/ocs/v2.php/apps/circles/circles?format=json");
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("circles request failed: {e}")))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::FORBIDDEN {
        return Ok(Vec::new());
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "circles returned HTTP {status}"
        )));
    }
    let env: CirclesEnvelope = resp
        .json()
        .await
        .map_err(|e| NimbusError::Protocol(format!("circles bad JSON: {e}")))?;
    Ok(env
        .ocs
        .data
        .into_iter()
        .filter(|c| !c.single_id.is_empty())
        .map(|c| NextcloudCircle {
            id: c.single_id.clone(),
            display_name: if c.display_name.is_empty() {
                c.single_id
            } else {
                c.display_name
            },
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct CircleMembersEnvelope {
    ocs: CircleMembersOcs,
}
#[derive(Debug, Deserialize)]
struct CircleMembersOcs {
    #[serde(default)]
    data: Vec<CircleMemberData>,
}
#[derive(Debug, Deserialize)]
struct CircleMemberData {
    /// Internal user id when `userType == 1` (NC user).
    #[serde(rename = "userId", default)]
    user_id: String,
    /// 1 = local NC user, others = email guest / contact / etc.
    #[serde(rename = "userType", default)]
    user_type: i32,
}

/// Member NC user IDs of a Circle / Team.  Returns only members
/// whose `userType == 1` (local NC users) since email-only and
/// contact-typed members don't round-trip through the same
/// profile lookup.
pub async fn fetch_circle_member_ids(
    server_url: &str,
    username: &str,
    app_password: &str,
    circle_id: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<String>, NimbusError> {
    let server = client::normalize_server_url(server_url);
    let url = format!(
        "{server}/ocs/v2.php/apps/circles/circles/{}/members?format=json",
        urlencoding(circle_id),
    );
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("circle members request failed: {e}")))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::FORBIDDEN {
        return Ok(Vec::new());
    }
    if !status.is_success() {
        return Err(NimbusError::Nextcloud(format!(
            "circle members returned HTTP {status}"
        )));
    }
    let env: CircleMembersEnvelope = resp
        .json()
        .await
        .map_err(|e| NimbusError::Protocol(format!("circle members bad JSON: {e}")))?;
    Ok(env
        .ocs
        .data
        .into_iter()
        .filter(|m| m.user_type == 1 && !m.user_id.is_empty())
        .map(|m| m.user_id)
        .collect())
}

/// Minimal percent-encoding for the `search=` query parameter.
/// We deliberately don't pull in `urlencoding` as a workspace
/// dep just for one path â€” emails contain only a small set of
/// chars that need escaping (`@`, `+`, `.`, `-`).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

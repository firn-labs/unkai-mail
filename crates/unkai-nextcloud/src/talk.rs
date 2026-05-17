//! Nextcloud Talk (spreed) integration â€” list and create Talk rooms,
//! and add participants to existing rooms.
//!
//! # Endpoint shape
//!
//! Talk lives under the OCS API at `/ocs/v2.php/apps/spreed/api/v4/`.
//! All calls share the same OCS etiquette as `shares.rs`:
//! `OCS-APIRequest: true` + `Accept: application/json`, Basic auth
//! with the app password, and an envelope of the form
//!
//! ```json
//! { "ocs": { "meta": { ... }, "data": { ... } } }
//! ```
//!
//! On success `data` is the payload object (or array, for `list_rooms`).
//! On failure `data` is typically `[]` and the failure code lives in
//! `meta.statuscode` *even though HTTP itself returned 200* â€” same
//! pattern shares.rs has to handle.
//!
//! # MVP scope (issue #13)
//!
//! Three operations, enough to power "create a Talk room from an email
//! thread", "list rooms in the sidebar", and "share the room link":
//!
//! - [`list_rooms`] â€” every room the user is a participant of.
//! - [`create_room`] â€” create a group room with an arbitrary set of
//!   participants (Nextcloud users *and* email-only invitees).
//! - [`add_participant`] â€” used internally by `create_room`, but
//!   exposed so callers can also extend an existing room.
//! - [`rename_room`] â€” used by the Compose "Add Event" flow so the
//!   Talk room created up-front (with the email subject as a
//!   placeholder name) can be renamed to match the final event title
//!   the user typed in the editor.
//!
//! Editing the rest of room settings (set password / promote
//! moderator) is left to a future issue â€” the Talk web UI handles
//! those and Unkai opens rooms in the browser anyway.

use serde::{Deserialize, Serialize};

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;

use crate::client;

// â”€â”€ Talk's wire-level room-type enum â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
//
// Spreed encodes room type as a small integer. We keep the numbers
// for the POST payload but expose a readable enum to the rest of the
// app â€” the UI shouldn't have to remember that "2 = group".
const ROOM_TYPE_GROUP: u8 = 2;

/// Kind of Talk room. We only ever *create* groups, but we list and
/// display all four standard types â€” `Other` is a forwards-compat
/// catch-all for any future spreed addition (e.g. notes-to-self rooms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomType {
    OneToOne,
    Group,
    Public,
    Changelog,
    Other,
}

impl RoomType {
    fn from_wire(n: u8) -> RoomType {
        match n {
            1 => RoomType::OneToOne,
            2 => RoomType::Group,
            3 => RoomType::Public,
            4 => RoomType::Changelog,
            _ => RoomType::Other,
        }
    }
}

/// One Talk room as the UI cares about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TalkRoom {
    /// Stable room token; used in URLs and follow-up API calls.
    pub token: String,
    /// What the user sees in lists. The server falls back to a
    /// participant-list summary if no explicit name was set, so this
    /// is always non-empty.
    pub display_name: String,
    /// Room type (1:1, group, public, changelog, other).
    pub room_type: RoomType,
    /// Number of unread messages for the current user. Drives the
    /// sidebar badge.
    pub unread_messages: u32,
    /// Whether the unread set contains a mention (@user). The sidebar
    /// uses this for a stronger "ping" badge style.
    pub unread_mention: bool,
    /// Unix timestamp (seconds) of the last activity in the room.
    /// Sortable so the sidebar lists the most recent first.
    pub last_activity: i64,
    /// Browser URL â€” `{server}/call/{token}`. Cached here so the UI
    /// doesn't have to reconstruct it on every render and so the
    /// "Share link in email" action has a single value to drop.
    pub web_url: String,
    /// Talk 21+ "archived" flag.  Archived rooms still exist on the
    /// server (and the user can still join + read history) but the
    /// web UI dims them and pushes them to the bottom of the list.
    /// Older Nextcloud / Talk versions don't send this field; serde
    /// defaults it to `false`, which matches their UX (everything
    /// active).
    pub is_archived: bool,
}

/// Source of a new participant. Talk distinguishes between adding a
/// known Nextcloud user (by user id) and inviting a guest by email
/// address â€” the API uses different `source` values for each.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum ParticipantSource {
    /// Nextcloud user id on this server (e.g. `"alice"`).
    User(String),
    /// Email address â€” Talk emails the recipient an invite link.
    Email(String),
}

// â”€â”€ Wire format â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
//
// Same two-phase pattern as `shares.rs`: meta first (always an object),
// then `data` parsed into the concrete shape only after we've confirmed
// the server didn't reply with `data: []` on failure.

#[derive(Debug, Deserialize)]
struct OcsRaw {
    ocs: OcsBodyRaw,
}

#[derive(Debug, Deserialize)]
struct OcsBodyRaw {
    meta: OcsMeta,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OcsMeta {
    status: String,
    statuscode: u16,
    #[serde(default)]
    message: Option<String>,
}

/// Spreed's room object as it appears on the wire â€” only the fields
/// we surface to the UI. The real object has 30+ fields (callFlags,
/// guestList, lobbyState, â€¦); the rest are dropped via the implicit
/// serde tolerance for unknown fields.
#[derive(Debug, Deserialize)]
struct WireRoom {
    token: String,
    #[serde(rename = "type")]
    room_type: u8,
    #[serde(rename = "displayName", default)]
    display_name: String,
    #[serde(rename = "unreadMessages", default)]
    unread_messages: u32,
    #[serde(rename = "unreadMention", default)]
    unread_mention: bool,
    #[serde(rename = "lastActivity", default)]
    last_activity: i64,
    #[serde(rename = "isArchived", default)]
    is_archived: bool,
}

impl WireRoom {
    fn into_room(self, server: &str) -> TalkRoom {
        TalkRoom {
            // `/call/{token}` is the canonical user-facing URL on every
            // standard Nextcloud install. Subpath installs expose the
            // same route; index.php prefixes are auto-handled by NC's
            // URL rewriter.
            web_url: format!("{server}/call/{}", self.token),
            token: self.token,
            display_name: self.display_name,
            room_type: RoomType::from_wire(self.room_type),
            unread_messages: self.unread_messages,
            unread_mention: self.unread_mention,
            last_activity: self.last_activity,
            is_archived: self.is_archived,
        }
    }
}

// â”€â”€ Public API â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// List every Talk room the current user is a participant of.
///
/// Returns an empty list (not an error) on a freshly installed Talk
/// where the user hasn't joined any rooms yet â€” the UI renders the
/// "Create your first room" empty state in that case.
pub async fn list_rooms(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<TalkRoom>, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}/ocs/v2.php/apps/spreed/api/v4/room?format=json");

    tracing::debug!("GET {url}");
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("talk list request failed: {e}")))?;

    let body = ocs_text(resp, "talk list rooms").await?;
    let rooms: Vec<WireRoom> = parse_ocs_data(&body, "talk list rooms")?;
    Ok(rooms.into_iter().map(|r| r.into_room(&server)).collect())
}

/// Create a new group Talk room and add `participants` to it.
///
/// Talk's create endpoint can take a single `invite` value to seed one
/// participant in one round-trip, but we always create empty and add
/// participants via [`add_participant`] afterwards â€” that lets us
/// support email-source participants uniformly, which is the
/// "create from email thread" path's main use case.
///
/// On a partial failure (room created, but adding the *n*th participant
/// failed) we surface the participant error and rely on the user to
/// finish the join in the browser. The room itself is preserved on the
/// server â€” leaving an empty room dangling is better than rolling back
/// and silently dropping a working room the user can still use.
/// Optional knobs to `create_room` that group together cleanly
/// â€” bundling them keeps the call signature within clippy's
/// `too_many_arguments` ceiling and clarifies that they're all
/// "tweaks for event-bound rooms" rather than required params.
///
/// `room_type`: `2` = group/private (only invited NC users can
/// join), `3` = public (anyone with the URL joins as guest).
/// `None` falls back to group/private â€” the Compose-side
/// "create Talk room" flow's behaviour.  Event-bound rooms pass
/// `Some(3)` so externals invited to the event can click
/// through the calendar link without an NC login.
///
/// `object_type` + `object_id` mirror Nextcloud Calendar's
/// "Make it a Talk conversation" tagging â€” pass
/// `Some("event")` and a unique id (typically `crypto.randomUUID()`)
/// to have Talk's UI categorise the room as a meeting room.
#[derive(Debug, Default)]
pub struct CreateRoomOptions<'a> {
    pub room_type: Option<u8>,
    pub object_type: Option<&'a str>,
    pub object_id: Option<&'a str>,
}

pub async fn create_room(
    server_url: &str,
    username: &str,
    app_password: &str,
    room_name: &str,
    participants: &[ParticipantSource],
    options: CreateRoomOptions<'_>,
    trusted_certs: &[TrustedCert],
) -> Result<TalkRoom, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}/ocs/v2.php/apps/spreed/api/v4/room?format=json");

    tracing::debug!("POST {url} (room_name={room_name:?})");
    let http = client::build(trusted_certs)?;
    let room_type = options.room_type.unwrap_or(ROOM_TYPE_GROUP).to_string();
    let mut form: Vec<(&str, &str)> =
        vec![("roomType", room_type.as_str()), ("roomName", room_name)];
    if let Some(t) = options.object_type {
        form.push(("objectType", t));
    }
    if let Some(id) = options.object_id {
        form.push(("objectId", id));
    }
    let resp = http
        .post(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .form(&form)
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("talk create request failed: {e}")))?;

    let body = ocs_text(resp, "talk create room").await?;
    let wire: WireRoom = parse_ocs_data(&body, "talk create room")?;
    let room = wire.into_room(&server);

    // Add participants serially. Talk rooms are typically small (single
    // digits of participants) so wall-clock cost is negligible, and a
    // serial loop keeps the error path simple â€” first failure wins,
    // already-added participants stay added.
    for p in participants {
        add_participant(
            server_url,
            username,
            app_password,
            &room.token,
            p,
            trusted_certs,
        )
        .await?;
    }

    Ok(room)
}

/// Add a single participant to an existing Talk room. Used by
/// [`create_room`] to fill out the participant list, and exposed
/// publicly so a future "Add participant" action can reuse it.
pub async fn add_participant(
    server_url: &str,
    username: &str,
    app_password: &str,
    room_token: &str,
    participant: &ParticipantSource,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!(
        "{server}/ocs/v2.php/apps/spreed/api/v4/room/{room_token}/participants?format=json"
    );

    let (new_participant, source) = match participant {
        ParticipantSource::User(id) => (id.as_str(), "users"),
        ParticipantSource::Email(addr) => (addr.as_str(), "emails"),
    };

    tracing::debug!("POST {url} (source={source})");
    let http = client::build(trusted_certs)?;
    let resp = http
        .post(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .form(&[("newParticipant", new_participant), ("source", source)])
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("talk add-participant request failed: {e}")))?;

    let body = ocs_text(resp, "talk add participant").await?;
    // Discard the participant payload â€” we just need the meta-level
    // success signal to know the add stuck.
    let _: serde_json::Value = parse_ocs_data(&body, "talk add participant")?;
    Ok(())
}

/// Rename an existing Talk room. Used by the Compose "Add Event"
/// flow: we create the room up-front (so its URL can prefill the
/// event editor's URL field) using the email subject as a placeholder
/// name, then rename the room to the final event title once the user
/// saves the event. The endpoint is the same `PUT /room/{token}` the
/// Talk web UI hits when editing the room name.
pub async fn rename_room(
    server_url: &str,
    username: &str,
    app_password: &str,
    room_token: &str,
    new_name: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}/ocs/v2.php/apps/spreed/api/v4/room/{room_token}?format=json");

    tracing::debug!("PUT {url} (new_name={new_name:?})");
    let http = client::build(trusted_certs)?;
    let resp = http
        .put(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .form(&[("roomName", new_name)])
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("talk rename request failed: {e}")))?;

    let body = ocs_text(resp, "talk rename room").await?;
    // Discard the updated-room payload â€” we already track the room
    // locally and a rename doesn't change anything else we care about.
    let _: serde_json::Value = parse_ocs_data(&body, "talk rename room")?;
    Ok(())
}

/// Toggle a Talk room's public-vs-group state.  `true` makes
/// the room public (anyone with the URL joins as a guest);
/// `false` reverts it to a group/private room (only invited
/// NC users can join).  Used by the EventEditor save flow:
/// rooms start public so externals can join via the calendar
/// invite, and we downgrade to private when every attendee
/// turns out to be an internal NC user.
///
/// Talk exposes this as two separate verbs on
/// `/room/{token}/public` â€” POST to enable, DELETE to disable.
/// We pick based on the boolean and emit the matching request.
pub async fn set_room_public(
    server_url: &str,
    username: &str,
    app_password: &str,
    room_token: &str,
    public: bool,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url =
        format!("{server}/ocs/v2.php/apps/spreed/api/v4/room/{room_token}/public?format=json");

    let http = client::build(trusted_certs)?;
    let req = if public {
        tracing::debug!("POST {url} (-> public)");
        http.post(&url)
    } else {
        tracing::debug!("DELETE {url} (-> private)");
        http.delete(&url)
    };
    let resp = req
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("talk set-public request failed: {e}")))?;

    let body = ocs_text(resp, "talk set room public").await?;
    let _: serde_json::Value = parse_ocs_data(&body, "talk set room public")?;
    Ok(())
}

/// Delete a Talk room.  Used by Compose's "discard draft" flow
/// (#86): rooms minted at compose-time get torn down when the user
/// cancels the draft so the Nextcloud Talk list doesn't accumulate
/// orphaned empty rooms.  The endpoint is the same `DELETE
/// /room/{token}` the Talk web UI hits when the moderator clicks
/// "Delete conversation".
pub async fn delete_room(
    server_url: &str,
    username: &str,
    app_password: &str,
    room_token: &str,
    trusted_certs: &[TrustedCert],
) -> Result<(), UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}/ocs/v2.php/apps/spreed/api/v4/room/{room_token}?format=json");

    tracing::debug!("DELETE {url}");
    let http = client::build(trusted_certs)?;
    let resp = http
        .delete(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("talk delete request failed: {e}")))?;

    // Talk returns the (now-deleted) room shape on success and an
    // OCS error envelope on failure; we just need the meta-level
    // success check `ocs_text` provides.
    let _ = ocs_text(resp, "talk delete room").await?;
    Ok(())
}

// â”€â”€ HTTP / parsing helpers â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Centralise the auth-failure / non-2xx handling so the three call
/// sites above stay focused on their request shape.
async fn ocs_text(resp: reqwest::Response, ctx: &str) -> Result<String, UnkaiError> {
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "{ctx} returned HTTP {status}"
        )));
    }
    resp.text()
        .await
        .map_err(|e| UnkaiError::Network(format!("{ctx} body read failed: {e}")))
}

/// Parse the OCS envelope and surface either the typed payload or a
/// meaningful error. Mirrors `shares::parse_share_response` â€” meta is
/// inspected first because on OCS-level failures `data` may be `[]`,
/// which would never deserialize into a payload struct.
fn parse_ocs_data<T: serde::de::DeserializeOwned>(body: &str, ctx: &str) -> Result<T, UnkaiError> {
    let raw: OcsRaw = serde_json::from_str(body)
        .map_err(|e| UnkaiError::Protocol(format!("{ctx} bad JSON: {e}")))?;

    if raw.ocs.meta.status != "ok" || raw.ocs.meta.statuscode >= 400 {
        let msg = raw
            .ocs
            .meta
            .message
            .unwrap_or_else(|| "rejected by server".to_string());
        return Err(UnkaiError::Nextcloud(format!(
            "{ctx} failed (OCS {}): {}",
            raw.ocs.meta.statuscode, msg
        )));
    }

    serde_json::from_value(raw.ocs.data)
        .map_err(|e| UnkaiError::Protocol(format!("{ctx} data bad shape: {e}")))
}

// â”€â”€ Tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-shape (trimmed) Nextcloud 28 list-rooms response. The actual
    /// per-room object has 30+ fields; we only assert on the ones we
    /// surface to the UI.
    const LIST_OK: &str = r#"{
      "ocs": {
        "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
        "data": [
          {
            "token": "abc123",
            "type": 2,
            "displayName": "Project sync",
            "unreadMessages": 4,
            "unreadMention": true,
            "lastActivity": 1745251200
          },
          {
            "token": "def456",
            "type": 1,
            "displayName": "alice",
            "unreadMessages": 0,
            "unreadMention": false,
            "lastActivity": 1745164800
          }
        ]
      }
    }"#;

    #[test]
    fn parses_list_rooms() {
        let rooms: Vec<WireRoom> = parse_ocs_data(LIST_OK, "test").unwrap();
        assert_eq!(rooms.len(), 2);

        let r0 = &rooms[0];
        assert_eq!(r0.token, "abc123");
        assert_eq!(r0.room_type, 2);
        assert_eq!(r0.display_name, "Project sync");
        assert_eq!(r0.unread_messages, 4);
        assert!(r0.unread_mention);

        let mapped = rooms[0]
            .clone_for_test()
            .into_room("https://cloud.example.com");
        assert_eq!(mapped.web_url, "https://cloud.example.com/call/abc123");
        assert_eq!(mapped.room_type, RoomType::Group);
    }

    #[test]
    fn parses_create_room() {
        let body = r#"{
          "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": {
              "token": "newtok",
              "type": 2,
              "displayName": "Standup",
              "unreadMessages": 0,
              "unreadMention": false,
              "lastActivity": 0
            }
          }
        }"#;
        let wire: WireRoom = parse_ocs_data(body, "test").unwrap();
        let room = wire.into_room("https://cloud.example.com");
        assert_eq!(room.token, "newtok");
        assert_eq!(room.web_url, "https://cloud.example.com/call/newtok");
        assert_eq!(room.room_type, RoomType::Group);
    }

    /// Talk denied the create (e.g. user lacks the `talk:create` right):
    /// HTTP 200 but `statuscode: 403` and `data: []`. Surface as a
    /// Nextcloud error with the server's message.
    #[test]
    fn surfaces_ocs_failure() {
        let body = r#"{
          "ocs": {
            "meta": {
              "status": "failure",
              "statuscode": 403,
              "message": "Permission denied"
            },
            "data": []
          }
        }"#;
        let err = parse_ocs_data::<WireRoom>(body, "test create").unwrap_err();
        match err {
            UnkaiError::Nextcloud(msg) => {
                assert!(msg.contains("403"), "expected status 403 in error: {msg}");
                assert!(
                    msg.contains("Permission denied"),
                    "expected server msg: {msg}"
                );
            }
            other => panic!("expected Nextcloud error, got {other:?}"),
        }
    }

    #[test]
    fn surfaces_bad_json_as_protocol_error() {
        let err = parse_ocs_data::<WireRoom>("not json", "test").unwrap_err();
        assert!(matches!(err, UnkaiError::Protocol(_)));
    }

    #[test]
    fn unknown_room_type_falls_back_to_other() {
        assert_eq!(RoomType::from_wire(99), RoomType::Other);
        assert_eq!(RoomType::from_wire(1), RoomType::OneToOne);
        assert_eq!(RoomType::from_wire(4), RoomType::Changelog);
    }

    // Helper because WireRoom doesn't derive Clone in the production
    // code â€” we don't need it outside tests.
    impl WireRoom {
        fn clone_for_test(&self) -> WireRoom {
            WireRoom {
                token: self.token.clone(),
                room_type: self.room_type,
                display_name: self.display_name.clone(),
                unread_messages: self.unread_messages,
                unread_mention: self.unread_mention,
                last_activity: self.last_activity,
                is_archived: self.is_archived,
            }
        }
    }
}

//! MCP Nextcloud Talk tools (#441): `list_talk_rooms` (read,
//! default on) and `create_talk_room` (write, default off).
//!
//! Talk is OCS-only, so both tools are gated on a connected real
//! Nextcloud whose capability snapshot includes the Talk app.
//! Rooms aren't cached locally (the in-app sidebar polls too), so
//! the list tool is a live OCS call.
//!
//! ## Guest-email side effect
//!
//! Adding a participant that is not a user on the Nextcloud makes
//! Talk email that address a guest-invite link **immediately** —
//! that's a server behaviour, not something Unkai can hold back.
//! `create_talk_room` therefore classifies each requested
//! participant via the sharees lookup first (user id where
//! possible — no email goes out for those) and its description
//! warns about the rest.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{Value, json};
use unkai_core::models::NextcloudAccount;
use unkai_nextcloud::{CreateRoomOptions, ParticipantSource, RoomType, TalkRoom};

use crate::nc::{nc_password, resolve_nc_account};
use crate::registry::{NextcloudFeature, ToolAccess, ToolContext, ToolDescriptor, ToolRegistry};
use crate::util::{internal, json_result, optional_bool, optional_str_list, required_str, schema};

pub(crate) fn register_talk_tools(registry: &mut ToolRegistry) {
    registry.register(
        ToolDescriptor {
            id: "list_talk_rooms",
            category: "talk",
            access: ToolAccess::Read,
            requires: Some(NextcloudFeature::Talk),
            description:
                "List the user's Nextcloud Talk conversations, including each room's join \
                 web_url.",
        },
        schema(json!({
            "type": "object",
            "properties": {
                "nextcloud_account_id": {
                    "type": "string",
                    "description": "Which connected Nextcloud to ask. Only needed when several offer Talk."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(list_talk_rooms(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "create_talk_room",
            category: "talk",
            access: ToolAccess::Write,
            requires: Some(NextcloudFeature::Talk),
            description:
                "Create a Nextcloud Talk room and return its join web_url. Participants are \
                 matched against the Nextcloud's users by email; anyone without an account \
                 there is added as a guest, and IMPORTANT: the server immediately emails \
                 guests an invite link when they are added. Set public=true for a room \
                 joinable by anyone with the link.",
        },
        schema(json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Display name of the new room."
                },
                "participant_emails": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "People to add. Nextcloud users are added silently; everyone else gets a guest-invite email from the server right away."
                },
                "public": {
                    "type": "boolean",
                    "description": "Anyone with the link may join (guest access). Default false: participants only."
                },
                "nextcloud_account_id": {
                    "type": "string",
                    "description": "Which connected Nextcloud to create the room on. Only needed when several offer Talk."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(create_talk_room(ctx, args))),
    );
}

fn room_json(room: &TalkRoom) -> Value {
    json!({
        "token": room.token,
        "name": room.display_name,
        "type": match room.room_type {
            RoomType::OneToOne => "one-to-one",
            RoomType::Group => "group",
            RoomType::Public => "public",
            RoomType::Changelog => "changelog",
            RoomType::Other => "other",
        },
        "web_url": room.web_url,
        "is_archived": room.is_archived,
    })
}

/// Classify participant emails into Nextcloud users vs. guest
/// emails via the sharees lookup — the same promotion the in-app
/// flow does so internal people don't get a needless guest-invite
/// email.  Fail-soft: an unreachable lookup degrades to the guest
/// path rather than failing the room.
pub(crate) async fn classify_participants(
    account: &NextcloudAccount,
    app_password: &str,
    emails: &[String],
) -> Vec<ParticipantSource> {
    let mut cache: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(emails.len());
    for email in emails {
        let key = email.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let user_id = match cache.get(&key) {
            Some(hit) => hit.clone(),
            None => {
                let looked_up = match unkai_nextcloud::find_user_by_email(
                    &account.server_url,
                    &account.username,
                    app_password,
                    email,
                    &account.trusted_certs,
                )
                .await
                {
                    Ok(m) => m.map(|m| m.user_id),
                    Err(e) => {
                        tracing::info!("MCP: sharees lookup for '{email}' failed: {e}");
                        None
                    }
                };
                cache.insert(key, looked_up.clone());
                looked_up
            }
        };
        out.push(match user_id {
            Some(id) => ParticipantSource::User(id),
            None => ParticipantSource::Email(email.trim().to_string()),
        });
    }
    out
}

// ── Handlers ────────────────────────────────────────────────────

async fn list_talk_rooms(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let account = resolve_nc_account(&ctx, &args, NextcloudFeature::Talk)?;
    let app_password = nc_password(&account)?;
    let rooms = unkai_nextcloud::list_rooms(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await
    .map_err(|e| internal(format!("Talk room listing failed: {e}")))?;

    Ok(json_result(json!({
        "nextcloud_account_id": account.id,
        "result_count": rooms.len(),
        "rooms": rooms.iter().map(room_json).collect::<Vec<_>>(),
    })))
}

async fn create_talk_room(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let name = required_str(&args, "name")?;
    let participant_emails = optional_str_list(&args, "participant_emails")?;
    let public = optional_bool(&args, "public")?.unwrap_or(false);
    let account = resolve_nc_account(&ctx, &args, NextcloudFeature::Talk)?;
    let app_password = nc_password(&account)?;

    let participants = classify_participants(&account, &app_password, &participant_emails).await;
    let guest_count = participants
        .iter()
        .filter(|p| matches!(p, ParticipantSource::Email(_)))
        .count();

    let room = unkai_nextcloud::create_room(
        &account.server_url,
        &account.username,
        &app_password,
        &name,
        &participants,
        CreateRoomOptions {
            // Talk wire values: 3 = public (joinable via link),
            // 2 = group (participants only).
            room_type: Some(if public { 3 } else { 2 }),
            object_type: None,
            object_id: None,
        },
        &account.trusted_certs,
    )
    .await
    .map_err(|e| internal(format!("Talk room creation failed: {e}")))?;

    let mut result = json!({
        "status": "room_created",
        "nextcloud_account_id": account.id,
        "room": room_json(&room),
    });
    if guest_count > 0 {
        result["note"] = json!(format!(
            "{guest_count} participant(s) have no account on the Nextcloud and were added \
             as guests — the server has emailed them an invite link."
        ));
    }
    Ok(json_result(result))
}

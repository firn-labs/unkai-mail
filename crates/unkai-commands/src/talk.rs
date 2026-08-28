//! Nextcloud Talk rooms and participants.
//!
//! Mirrors `ui/src/lib/api/talk.ts`.

use unkai_core::UnkaiError;
use unkai_store::Cache;
use unkai_store::credentials;

use crate::support::load_nextcloud_account;

// ── Nextcloud Talk ──────────────────────────────────────────────
//
// Three commands, mirroring the file/share pattern: each call loads
// the account + app password from local state and forwards to the
// matching `unkai_nextcloud::talk::*` function. We don't cache the
// room list — Talk's `/room` is cheap and unread counts go stale the
// moment a colleague sends a message anyway. The sidebar polls on a
// timer instead.

/// List every Talk room the connected Nextcloud user is a participant
/// of. Drives the sidebar's "Talk Rooms" group.
pub async fn list_talk_rooms(
    nc_id: String,
    cache: &Cache,
) -> Result<Vec<unkai_nextcloud::TalkRoom>, UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::list_rooms(
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await
}

/// Create a new group Talk room and invite `participants` to it.
///
/// `participants` carries a tagged enum (`{kind: "user"|"email", value: ...}`)
/// per invitee — `kind=email` triggers Talk's guest-invite flow so
/// recipients without a Nextcloud account get an emailed link. The
/// frontend builds this list from the email's To/Cc by treating
/// addresses matching the connected NC server's user list as `user`
/// and the rest as `email`. (For the MVP we always send `email` and
/// let Talk match users on the server side.)
// `object_type` / `object_id` mirror Nextcloud Calendar's "Make
// it a Talk conversation" flow — pass `objectType: "event"` plus
// any random unique id to have Talk categorise the room as a
// meeting room.  Plain Compose-side "create Talk room" flows
// leave both `None`.
//
// `room_type` controls who can join: `2` = group/private (NC
// users only), `3` = public (anyone with the URL joins as
// guest).  Event-bound rooms default to `3` so externals
// invited via the calendar invite can click through without
// hitting the NC login wall.
pub async fn create_talk_room(
    nc_id: String,
    room_name: String,
    participants: Vec<unkai_nextcloud::ParticipantSource>,
    object_type: Option<String>,
    object_id: Option<String>,
    room_type: Option<u8>,
    cache: &Cache,
) -> Result<unkai_nextcloud::TalkRoom, UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::create_room(
        &account.server_url,
        &account.username,
        &app_password,
        &room_name,
        &participants,
        unkai_nextcloud::CreateRoomOptions {
            room_type,
            object_type: object_type.as_deref(),
            object_id: object_id.as_deref(),
        },
        &account.trusted_certs,
    )
    .await
}

/// Toggle a Talk room's public/private visibility.  Used by
/// the EventEditor save flow to downgrade a room from public
/// to private once we've confirmed every attendee is an
/// internal NC user — the externals-only flag is no longer
/// needed and the room shouldn't be join-by-URL after that
/// point.
pub async fn set_talk_room_public(
    nc_id: String,
    room_token: String,
    public: bool,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::set_room_public(
        &account.server_url,
        &account.username,
        &app_password,
        &room_token,
        public,
        &account.trusted_certs,
    )
    .await
}

/// Promote an `Email`-source participant to a `User`-source one
/// whenever the address belongs to a real Nextcloud account on
/// this server (issue #124).  The internal user lands in the
/// room as themselves with an in-NC notification instead of
/// receiving a guest invite link via email — better UX, native
/// rights, and no second mail in the recipient's inbox.
///
/// Lookup is fail-soft: a network blip or an admin-restricted
/// sharees endpoint falls through to the original `Email`
/// source so the invite still gets out, just as a guest.  An
/// in-batch cache (`HashMap<lowercased-addr, ParticipantSource>`)
/// keeps duplicate addresses across the To/Cc list to a single
/// OCS round-trip.
pub async fn promote_email_to_user_if_internal(
    server_url: &str,
    username: &str,
    app_password: &str,
    src: &unkai_nextcloud::ParticipantSource,
    cache: &mut std::collections::HashMap<String, unkai_nextcloud::ParticipantSource>,
) -> unkai_nextcloud::ParticipantSource {
    use unkai_nextcloud::ParticipantSource;
    let addr = match src {
        ParticipantSource::User(_) => return src.clone(),
        ParticipantSource::Email(a) => a,
    };
    let key = addr.to_lowercase();
    if let Some(hit) = cache.get(&key) {
        return hit.clone();
    }
    let resolved =
        match unkai_nextcloud::find_user_by_email(server_url, username, app_password, addr, &[])
            .await
        {
            Ok(Some(m)) => ParticipantSource::User(m.user_id),
            Ok(None) => src.clone(),
            Err(e) => {
                tracing::warn!(
                    "talk-invite: NC user lookup failed for {addr}: {e}; \
                 falling back to email guest"
                );
                src.clone()
            }
        };
    cache.insert(key, resolved.clone());
    resolved
}

/// Add a single participant to an existing Talk room. Exposed so the
/// UI can grow an "Add participant" affordance later without a
/// backend round-trip.  Email-source participants whose address
/// matches a Nextcloud user on this server are silently promoted
/// to `User` source (issue #124).
pub async fn add_talk_participant(
    nc_id: String,
    room_token: String,
    participant: unkai_nextcloud::ParticipantSource,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    // Lookup memo for the promotion helper — deliberately not
    // named `cache` so it can't be mistaken for the DB handle.
    let mut lookup_memo = std::collections::HashMap::new();
    let resolved = promote_email_to_user_if_internal(
        &account.server_url,
        &account.username,
        &app_password,
        &participant,
        &mut lookup_memo,
    )
    .await;
    unkai_nextcloud::add_participant(
        &account.server_url,
        &account.username,
        &app_password,
        &room_token,
        &resolved,
        &account.trusted_certs,
    )
    .await
}

/// Batched add — invite a whole list of participants on a single
/// auth handshake.  Used by Compose's deferred-invite flow (#86):
/// we create the Talk room empty at compose-time and only invite
/// the recipients once `Send` actually goes out, so a discarded
/// draft doesn't leave a room full of strangers in the recipient's
/// Talk list.  Sequential (not parallel) so the first failure halts
/// the batch and surfaces as a single error.  Email-source entries
/// whose address matches a Nextcloud user on this server are
/// promoted to `User` source per issue #124 — internal recipients
/// join natively, externals still get the email-guest flow.
pub async fn add_talk_participants(
    nc_id: String,
    room_token: String,
    participants: Vec<unkai_nextcloud::ParticipantSource>,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    // Lookup memo for the promotion helper — deliberately not
    // named `cache` so it can't be mistaken for the DB handle.
    let mut lookup_memo = std::collections::HashMap::new();
    for p in &participants {
        let resolved = promote_email_to_user_if_internal(
            &account.server_url,
            &account.username,
            &app_password,
            p,
            &mut lookup_memo,
        )
        .await;
        unkai_nextcloud::add_participant(
            &account.server_url,
            &account.username,
            &app_password,
            &room_token,
            &resolved,
            &account.trusted_certs,
        )
        .await?;
    }
    Ok(())
}

/// Tear down a Talk room (#86).  Compose's `cancel` flow calls this
/// whenever the user discards a draft that minted a room earlier
/// in the session — without it, the room would dangle empty in the
/// user's Talk list with no context.
pub async fn delete_talk_room(
    nc_id: String,
    room_token: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::delete_room(
        &account.server_url,
        &account.username,
        &app_password,
        &room_token,
        &account.trusted_certs,
    )
    .await
}

/// Rename an existing Talk room. Used by the Compose "Add Event"
/// flow to keep the auto-created Talk room's name in sync with the
/// final event title once the user saves the event.
pub async fn rename_talk_room(
    nc_id: String,
    room_token: String,
    new_name: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::rename_room(
        &account.server_url,
        &account.username,
        &app_password,
        &room_token,
        &new_name,
        &account.trusted_certs,
    )
    .await
}

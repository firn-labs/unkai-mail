//! The `create_meeting_invite` composite tool (#441).
//!
//! Orchestrates the full "set up a meeting" flow in one call:
//!
//! 1. *(optional)* create a **public Talk room** (no participants
//!    — recipients join via the link, so no guest-invite emails
//!    fire here),
//! 2. create the **calendar event** with the attendees on the ICS
//!    — this is the step where the user's server sends the iMIP
//!    invitation emails,
//! 3. save the **invite-card draft** (the same styled card the
//!    in-app flows produce, ported in [`crate::invite_html`]) into
//!    the mail account's Drafts folder for the user to review and
//!    send from Unkai Mail.
//!
//! ## Rollback
//!
//! Each step's failure undoes the previous steps best-effort:
//! a failed event deletes the Talk room; a failed draft deletes
//! the event (the server then sends iMIP CANCEL notices — the
//! correct follow-up to invitations that already went out) and the
//! room.  The orchestration lives in [`orchestrate_meeting`],
//! separated from the network code behind the [`MeetingSteps`]
//! trait so the rollback ordering is unit-testable.
//!
//! The local-cache mirror of the event is written only after every
//! step succeeded, so rollback never has to touch the cache.

use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::json;
use unkai_core::models::{Account, NextcloudAccount};
use unkai_nextcloud::CreateRoomOptions;

use crate::calendar::{
    CreatedEvent, EventSpec, create_event_impl, rollback_created_event, upsert_created_event,
    validate_writable_calendar,
};
use crate::invite_html::{MeetingInviteCard, format_range, meeting_invite_html};
use crate::mail::{append_draft, build_draft_outgoing};
use crate::nc::{account_has_feature, load_nc_accounts, nc_password};
use crate::registry::{NextcloudFeature, ToolAccess, ToolContext, ToolDescriptor, ToolRegistry};
use crate::util::{
    internal, invalid, json_result, load_accounts, optional_bool, optional_str, required_datetime,
    required_str, required_str_list, schema,
};

pub(crate) fn register_meeting_tools(registry: &mut ToolRegistry) {
    registry.register(
        ToolDescriptor {
            id: "create_meeting_invite",
            category: "calendar",
            access: ToolAccess::Write,
            requires: Some(NextcloudFeature::Calendar),
            description:
                "Set up a complete meeting: optionally create a Nextcloud Talk room, create \
                 the calendar event, and save a styled invite-card email as a DRAFT in the \
                 mail account's Drafts folder. IMPORTANT: the server emails iMIP calendar \
                 invitations to all attendees as soon as the event is created — only the \
                 invite-card email itself stays a draft for the user to review and send \
                 from Unkai Mail. If a later step fails, earlier steps are rolled back \
                 (a cancelled event triggers iMIP cancellation notices).",
        },
        schema(json!({
            "type": "object",
            "required": ["account_id", "calendar_id", "summary", "start", "end", "attendees"],
            "properties": {
                "account_id": {
                    "type": "string",
                    "description": "Mail account whose Drafts folder receives the invite card (see list_accounts)."
                },
                "calendar_id": {
                    "type": "string",
                    "description": "Calendar for the event (see list_calendars; must not be read_only)."
                },
                "summary": {"type": "string", "description": "Meeting title."},
                "start": {"type": "string", "description": "RFC 3339 start time."},
                "end": {"type": "string", "description": "RFC 3339 end time."},
                "attendees": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "description": "Attendee email addresses — they receive the server's iMIP invitation and are the draft's recipients."
                },
                "description": {"type": "string", "description": "Agenda / notes for the event and the invite card."},
                "location": {"type": "string"},
                "include_talk_room": {
                    "type": "boolean",
                    "description": "Also create a public Talk room and link it from the event and the card. Default false."
                },
                "talk_room_name": {
                    "type": "string",
                    "description": "Name for the Talk room. Defaults to the meeting summary."
                },
                "subject": {
                    "type": "string",
                    "description": "Subject of the draft email. Defaults to 'Invitation: <summary>'."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(create_meeting_invite(ctx, args))),
    );
}

// ── Orchestration (pure, unit-testable) ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MeetingStep {
    TalkRoom,
    Event,
    Draft,
}

impl MeetingStep {
    fn label(self) -> &'static str {
        match self {
            MeetingStep::TalkRoom => "Talk room",
            MeetingStep::Event => "calendar event",
            MeetingStep::Draft => "invite draft",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RoomInfo {
    pub name: String,
    pub token: String,
    pub web_url: String,
}

#[derive(Debug, Clone)]
pub(crate) struct DraftInfo {
    pub folder: String,
    pub uid: Option<u32>,
}

pub(crate) struct MeetingOutcome {
    pub room: Option<RoomInfo>,
    pub event_id: String,
    pub draft: DraftInfo,
}

pub(crate) struct MeetingFailure {
    pub step: MeetingStep,
    pub error: String,
    /// What was undone afterwards, in undo order, with each
    /// rollback's own result (rollbacks are best-effort).
    pub rollbacks: Vec<(MeetingStep, Result<(), String>)>,
}

/// The three side-effecting steps plus their undo operations.
/// The live implementation talks to Talk/CalDAV/IMAP; tests
/// substitute a recorder to pin the rollback ordering.
pub(crate) trait MeetingSteps {
    async fn create_room(&mut self) -> Result<RoomInfo, String>;
    async fn rollback_room(&mut self, room: &RoomInfo) -> Result<(), String>;
    /// Returns the created event's id.  This is the step that
    /// makes the server send iMIP invitations.
    async fn create_event(&mut self, talk_url: Option<&str>) -> Result<String, String>;
    async fn rollback_event(&mut self) -> Result<(), String>;
    async fn create_draft(&mut self, room: Option<&RoomInfo>) -> Result<DraftInfo, String>;
}

/// Run the composite: room? → event → draft, undoing earlier
/// steps when a later one fails.  Undo order is reverse creation
/// order — event first (its CANCEL notices chase the invitations
/// the failed flow already sent), then the room.
pub(crate) async fn orchestrate_meeting(
    steps: &mut impl MeetingSteps,
    want_room: bool,
) -> Result<MeetingOutcome, MeetingFailure> {
    let room = if want_room {
        match steps.create_room().await {
            Ok(room) => Some(room),
            Err(error) => {
                return Err(MeetingFailure {
                    step: MeetingStep::TalkRoom,
                    error,
                    rollbacks: Vec::new(),
                });
            }
        }
    } else {
        None
    };

    let event_id = match steps
        .create_event(room.as_ref().map(|r| r.web_url.as_str()))
        .await
    {
        Ok(id) => id,
        Err(error) => {
            let mut rollbacks = Vec::new();
            if let Some(room) = &room {
                rollbacks.push((MeetingStep::TalkRoom, steps.rollback_room(room).await));
            }
            return Err(MeetingFailure {
                step: MeetingStep::Event,
                error,
                rollbacks,
            });
        }
    };

    match steps.create_draft(room.as_ref()).await {
        Ok(draft) => Ok(MeetingOutcome {
            room,
            event_id,
            draft,
        }),
        Err(error) => {
            let mut rollbacks = vec![(MeetingStep::Event, steps.rollback_event().await)];
            if let Some(room) = &room {
                rollbacks.push((MeetingStep::TalkRoom, steps.rollback_room(room).await));
            }
            Err(MeetingFailure {
                step: MeetingStep::Draft,
                error,
                rollbacks,
            })
        }
    }
}

// ── Live implementation ─────────────────────────────────────────

struct LiveSteps<'a> {
    ctx: &'a ToolContext,
    mail_account: Account,
    nc_account: NextcloudAccount,
    calendar_id: String,
    room_name: String,
    subject: String,
    summary: String,
    description: Option<String>,
    location: Option<String>,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    attendees: Vec<String>,
    /// Set by `create_event`, consumed by `rollback_event` and the
    /// post-orchestration cache commit.
    created_event: Option<CreatedEvent>,
}

impl LiveSteps<'_> {
    fn invite_card(&self, room: Option<&RoomInfo>) -> MeetingInviteCard {
        MeetingInviteCard {
            summary: self.summary.clone(),
            start: self.start,
            end: self.end,
            location: self.location.clone(),
            description: self.description.clone(),
            talk_url: room.map(|r| r.web_url.clone()),
        }
    }

    /// Plain-text alternative body for clients that don't render
    /// the HTML card.
    fn draft_text(&self, room: Option<&RoomInfo>) -> String {
        let when = format_range(
            &self.start.with_timezone(&chrono::Local),
            &self.end.with_timezone(&chrono::Local),
        );
        let mut lines = vec![self.summary.clone(), format!("When: {when}")];
        if let Some(location) = self.location.as_deref().filter(|l| !l.trim().is_empty()) {
            lines.push(format!("Where: {}", location.trim()));
        }
        if let Some(room) = room {
            lines.push(format!("Talk room: {}", room.web_url));
        }
        if let Some(description) = self.description.as_deref().filter(|d| !d.trim().is_empty()) {
            lines.push(String::new());
            lines.push(description.trim().to_string());
        }
        lines.join("\n")
    }
}

impl MeetingSteps for LiveSteps<'_> {
    async fn create_room(&mut self) -> Result<RoomInfo, String> {
        let app_password = nc_password(&self.nc_account).map_err(|e| e.message.to_string())?;
        // Public room (wire value 3), no participants: attendees
        // reach it via the link in the event and the card, so Talk
        // sends no guest-invite emails of its own here.
        let room = unkai_nextcloud::create_room(
            &self.nc_account.server_url,
            &self.nc_account.username,
            &app_password,
            &self.room_name,
            &[],
            CreateRoomOptions {
                room_type: Some(3),
                object_type: None,
                object_id: None,
            },
            &self.nc_account.trusted_certs,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(RoomInfo {
            name: room.display_name,
            token: room.token,
            web_url: room.web_url,
        })
    }

    async fn rollback_room(&mut self, room: &RoomInfo) -> Result<(), String> {
        let app_password = nc_password(&self.nc_account).map_err(|e| e.message.to_string())?;
        unkai_nextcloud::delete_room(
            &self.nc_account.server_url,
            &self.nc_account.username,
            &app_password,
            &room.token,
            &self.nc_account.trusted_certs,
        )
        .await
        .map_err(|e| e.to_string())
    }

    async fn create_event(&mut self, talk_url: Option<&str>) -> Result<String, String> {
        let spec = EventSpec {
            summary: self.summary.clone(),
            description: self.description.clone(),
            location: self.location.clone(),
            url: talk_url.map(str::to_string),
            start: self.start,
            end: self.end,
            all_day: false,
            attendees: self.attendees.clone(),
        };
        let created = create_event_impl(self.ctx, &self.calendar_id, spec)
            .await
            .map_err(|e| e.message.to_string())?;
        let event_id = created.event_id.clone();
        self.created_event = Some(created);
        Ok(event_id)
    }

    async fn rollback_event(&mut self) -> Result<(), String> {
        match self.created_event.take() {
            Some(created) => rollback_created_event(self.ctx, &created).await,
            None => Ok(()),
        }
    }

    async fn create_draft(&mut self, room: Option<&RoomInfo>) -> Result<DraftInfo, String> {
        let mut outgoing = build_draft_outgoing(
            &self.mail_account,
            self.attendees.clone(),
            Vec::new(),
            Vec::new(),
            self.subject.clone(),
            self.draft_text(room),
            None,
        );
        outgoing.body_html = Some(meeting_invite_html(&self.invite_card(room)));
        let saved = append_draft(self.ctx, &self.mail_account, &outgoing)
            .await
            .map_err(|e| e.message.to_string())?;
        Ok(DraftInfo {
            folder: saved.folder,
            uid: saved.uid,
        })
    }
}

// ── Handler ─────────────────────────────────────────────────────

async fn create_meeting_invite(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let account_id = required_str(&args, "account_id")?;
    let calendar_id = required_str(&args, "calendar_id")?;
    let summary = required_str(&args, "summary")?;
    let start = required_datetime(&args, "start")?;
    let end = required_datetime(&args, "end")?;
    let attendees = required_str_list(&args, "attendees")?;
    let description = optional_str(&args, "description")?;
    let location = optional_str(&args, "location")?;
    let include_talk_room = optional_bool(&args, "include_talk_room")?.unwrap_or(false);
    let room_name = optional_str(&args, "talk_room_name")?.unwrap_or_else(|| summary.clone());
    let subject =
        optional_str(&args, "subject")?.unwrap_or_else(|| format!("Invitation: {summary}"));

    if end <= start {
        return Err(invalid("end must be after start"));
    }
    for attendee in &attendees {
        if !attendee.contains('@') {
            return Err(invalid(format!(
                "attendee '{attendee}' is not an email address"
            )));
        }
    }

    // Validate every precondition BEFORE the first side effect —
    // a composite that fails on step three after creating a room
    // and emailing invitations is strictly worse than one that
    // refuses up front.
    let mail_account = load_accounts(&ctx)?
        .into_iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| {
            invalid(format!(
                "unknown account_id '{account_id}' — call list_accounts for valid ids"
            ))
        })?;
    if mail_account.use_jmap && mail_account.jmap_url.is_some() {
        return Err(ErrorData::invalid_request(
            "this account uses JMAP; creating drafts via MCP currently supports IMAP \
             accounts only",
            None,
        ));
    }
    let folders = ctx
        .cache
        .get_folders(&account_id)
        .map_err(|e| internal(format!("cache read failed: {e}")))?;
    if unkai_core::mail_util::pick_drafts_folder(&folders).is_none() {
        return Err(ErrorData::invalid_request(
            "no Drafts folder found in the account's synced folder list — open the account \
             in Unkai Mail once so its folders are synced",
            None,
        ));
    }

    let (nc_id, _) = validate_writable_calendar(&ctx, &calendar_id)?;
    let nc_account = load_nc_accounts(&ctx.cache)
        .into_iter()
        .find(|a| a.id == nc_id)
        .ok_or_else(|| {
            internal(format!(
                "the connected source '{nc_id}' referenced by the cache no longer exists"
            ))
        })?;
    if include_talk_room && !account_has_feature(&nc_account, NextcloudFeature::Talk) {
        return Err(invalid(
            "the calendar's Nextcloud has no Talk app — retry with include_talk_room=false",
        ));
    }

    let mut steps = LiveSteps {
        ctx: &ctx,
        mail_account,
        nc_account,
        calendar_id,
        room_name,
        subject,
        summary,
        description,
        location,
        start,
        end,
        attendees,
        created_event: None,
    };

    match orchestrate_meeting(&mut steps, include_talk_room).await {
        Ok(outcome) => {
            // Mirror the event into the local cache only now that
            // the whole composite stands — rollback never has to
            // clean the cache up.  A failed mirror is only a
            // staleness issue (the next sync fixes it), not worth
            // failing a composite whose real resources all exist.
            let mut cache_note = None;
            if let Some(created) = steps.created_event.as_ref()
                && let Err(e) = upsert_created_event(&ctx, created)
            {
                tracing::warn!("MCP: created event not mirrored into cache: {}", e.message);
                cache_note = Some(
                    "The event exists on the server but is not yet visible locally; it appears after the next calendar sync.",
                );
            }

            let mut result = json!({
                "status": "meeting_invite_created",
                "event_id": outcome.event_id,
                "draft": {
                    "account_id": steps.mail_account.id,
                    "folder": outcome.draft.folder,
                    "uid": outcome.draft.uid,
                },
                "note": "The server is emailing iMIP invitations to the attendees. The \
                         invite-card email is a DRAFT — the user reviews and sends it from \
                         Unkai Mail.",
            });
            if let Some(room) = &outcome.room {
                result["talk_room"] = json!({
                    "name": room.name,
                    "token": room.token,
                    "web_url": room.web_url,
                });
            }
            if let Some(note) = cache_note {
                result["cache_note"] = json!(note);
            }
            Ok(json_result(result))
        }
        Err(failure) => {
            let mut message = format!(
                "creating the {} failed: {}",
                failure.step.label(),
                failure.error
            );
            for (step, result) in &failure.rollbacks {
                match result {
                    Ok(()) => {
                        message.push_str(&format!("; the {} was rolled back", step.label()));
                    }
                    Err(e) => message.push_str(&format!(
                        "; rolling back the {} ALSO failed ({e}) — the user should check \
                         their {} manually",
                        step.label(),
                        match step {
                            MeetingStep::TalkRoom => "Talk rooms",
                            MeetingStep::Event => "calendar",
                            MeetingStep::Draft => "drafts",
                        },
                    )),
                }
            }
            Err(internal(message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorder implementation: scripts which steps fail and logs
    /// every call so the tests can pin the rollback ordering.
    struct MockSteps {
        fail_room: bool,
        fail_event: bool,
        fail_draft: bool,
        fail_rollback_event: bool,
        calls: Vec<&'static str>,
    }

    impl MockSteps {
        fn new() -> Self {
            Self {
                fail_room: false,
                fail_event: false,
                fail_draft: false,
                fail_rollback_event: false,
                calls: Vec::new(),
            }
        }
    }

    impl MeetingSteps for MockSteps {
        async fn create_room(&mut self) -> Result<RoomInfo, String> {
            self.calls.push("create_room");
            if self.fail_room {
                return Err("room boom".into());
            }
            Ok(RoomInfo {
                name: "Weekly sync".into(),
                token: "tok".into(),
                web_url: "https://cloud.example.com/call/tok".into(),
            })
        }

        async fn rollback_room(&mut self, _room: &RoomInfo) -> Result<(), String> {
            self.calls.push("rollback_room");
            Ok(())
        }

        async fn create_event(&mut self, talk_url: Option<&str>) -> Result<String, String> {
            self.calls.push(if talk_url.is_some() {
                "create_event_with_link"
            } else {
                "create_event"
            });
            if self.fail_event {
                return Err("event boom".into());
            }
            Ok("cal::evt".into())
        }

        async fn rollback_event(&mut self) -> Result<(), String> {
            self.calls.push("rollback_event");
            if self.fail_rollback_event {
                return Err("rollback boom".into());
            }
            Ok(())
        }

        async fn create_draft(&mut self, _room: Option<&RoomInfo>) -> Result<DraftInfo, String> {
            self.calls.push("create_draft");
            if self.fail_draft {
                return Err("draft boom".into());
            }
            Ok(DraftInfo {
                folder: "Drafts".into(),
                uid: Some(7),
            })
        }
    }

    #[tokio::test]
    async fn happy_path_runs_room_event_draft_without_rollbacks() {
        let mut steps = MockSteps::new();
        let outcome = orchestrate_meeting(&mut steps, true).await.ok().unwrap();
        assert_eq!(
            steps.calls,
            vec!["create_room", "create_event_with_link", "create_draft"]
        );
        assert!(outcome.room.is_some());
        assert_eq!(outcome.event_id, "cal::evt");
        assert_eq!(outcome.draft.uid, Some(7));
    }

    #[tokio::test]
    async fn without_room_the_talk_step_is_skipped() {
        let mut steps = MockSteps::new();
        let outcome = orchestrate_meeting(&mut steps, false).await.ok().unwrap();
        assert_eq!(steps.calls, vec!["create_event", "create_draft"]);
        assert!(outcome.room.is_none());
    }

    #[tokio::test]
    async fn failed_event_rolls_back_the_room() {
        let mut steps = MockSteps::new();
        steps.fail_event = true;
        let failure = orchestrate_meeting(&mut steps, true).await.err().unwrap();
        assert_eq!(
            steps.calls,
            vec!["create_room", "create_event_with_link", "rollback_room"]
        );
        assert_eq!(failure.step, MeetingStep::Event);
        assert_eq!(failure.rollbacks.len(), 1);
        assert_eq!(failure.rollbacks[0].0, MeetingStep::TalkRoom);
        assert!(failure.rollbacks[0].1.is_ok());
    }

    #[tokio::test]
    async fn failed_draft_rolls_back_event_then_room() {
        let mut steps = MockSteps::new();
        steps.fail_draft = true;
        let failure = orchestrate_meeting(&mut steps, true).await.err().unwrap();
        assert_eq!(
            steps.calls,
            vec![
                "create_room",
                "create_event_with_link",
                "create_draft",
                "rollback_event",
                "rollback_room"
            ]
        );
        assert_eq!(failure.step, MeetingStep::Draft);
        let steps_rolled: Vec<MeetingStep> = failure.rollbacks.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            steps_rolled,
            vec![MeetingStep::Event, MeetingStep::TalkRoom]
        );
    }

    #[tokio::test]
    async fn failed_rollback_is_reported_not_swallowed() {
        let mut steps = MockSteps::new();
        steps.fail_draft = true;
        steps.fail_rollback_event = true;
        let failure = orchestrate_meeting(&mut steps, false).await.err().unwrap();
        assert_eq!(failure.step, MeetingStep::Draft);
        assert_eq!(failure.rollbacks.len(), 1);
        assert_eq!(
            failure.rollbacks[0].1.as_ref().err().unwrap(),
            "rollback boom"
        );
    }

    #[tokio::test]
    async fn failed_room_aborts_before_any_other_step() {
        let mut steps = MockSteps::new();
        steps.fail_room = true;
        let failure = orchestrate_meeting(&mut steps, true).await.err().unwrap();
        assert_eq!(steps.calls, vec!["create_room"]);
        assert_eq!(failure.step, MeetingStep::TalkRoom);
        assert!(failure.rollbacks.is_empty());
    }
}

#[cfg(test)]
mod precheck_tests {
    use super::*;
    use crate::nc::test_support::{caps, nc_account};
    use crate::testutil::{invoke, mail_account, test_context};
    use unkai_core::models::{DavSourceKind, Folder};
    use unkai_store::cache::CalendarRow;
    use unkai_store::{account_store, nextcloud_store};

    fn base_args(calendar_id: &str) -> serde_json::Value {
        json!({
            "account_id": "mail-1",
            "calendar_id": calendar_id,
            "summary": "Planning",
            "start": "2026-08-01T09:00:00Z",
            "end": "2026-08-01T10:00:00Z",
            "attendees": ["jane@example.com"],
        })
    }

    fn seed_calendar(ctx: &crate::registry::ToolContext, read_only: bool) -> String {
        nextcloud_store::upsert_account(
            &ctx.cache,
            nc_account("acc", DavSourceKind::Local, Some(caps(false, true, false))),
        )
        .unwrap();
        ctx.cache
            .upsert_calendars(
                "acc",
                &[CalendarRow {
                    path: "local://acc/cal".into(),
                    display_name: "Personal".into(),
                    color: None,
                    ctag: None,
                    hidden: false,
                    muted: false,
                    read_only,
                }],
            )
            .unwrap();
        "acc::local://acc/cal".into()
    }

    fn seed_drafts_folder(ctx: &crate::registry::ToolContext) {
        ctx.cache
            .upsert_folders(
                "mail-1",
                &[Folder {
                    name: "Drafts".into(),
                    delimiter: Some("/".into()),
                    attributes: vec!["\\Drafts".into()],
                    unread_count: None,
                }],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn composite_prechecks_fire_before_any_side_effect() {
        let ctx = test_context();

        // Unknown mail account.
        let err = invoke(&ctx, "create_meeting_invite", base_args("acc::x"))
            .await
            .expect_err("unknown mail account should error");
        assert!(err.message.contains("unknown account_id"));

        // JMAP mail account is refused (drafts go via IMAP APPEND).
        let mut jmap = mail_account("mail-1");
        jmap.use_jmap = true;
        jmap.jmap_url = Some("https://mail.example.com/jmap".into());
        account_store::add_account(&ctx.cache, jmap).unwrap();
        let err = invoke(&ctx, "create_meeting_invite", base_args("acc::x"))
            .await
            .expect_err("JMAP account should error");
        assert!(err.message.contains("JMAP"));
    }

    #[tokio::test]
    async fn composite_requires_drafts_folder_and_writable_calendar() {
        let ctx = test_context();
        account_store::add_account(&ctx.cache, mail_account("mail-1")).unwrap();

        // No Drafts folder synced yet.
        let err = invoke(&ctx, "create_meeting_invite", base_args("acc::x"))
            .await
            .expect_err("missing Drafts folder should error");
        assert!(err.message.contains("Drafts"));

        // Read-only calendar is refused before the Talk-room step.
        seed_drafts_folder(&ctx);
        let calendar_id = seed_calendar(&ctx, true);
        let err = invoke(&ctx, "create_meeting_invite", base_args(&calendar_id))
            .await
            .expect_err("read-only calendar should error");
        assert!(err.message.contains("read-only"));
    }

    #[tokio::test]
    async fn composite_refuses_talk_room_when_the_source_has_no_talk() {
        let ctx = test_context();
        account_store::add_account(&ctx.cache, mail_account("mail-1")).unwrap();
        seed_drafts_folder(&ctx);
        let calendar_id = seed_calendar(&ctx, false);

        let mut args = base_args(&calendar_id);
        args["include_talk_room"] = json!(true);
        let err = invoke(&ctx, "create_meeting_invite", args)
            .await
            .expect_err("talk-less source should refuse include_talk_room");
        assert!(err.message.contains("include_talk_room=false"));
    }
}

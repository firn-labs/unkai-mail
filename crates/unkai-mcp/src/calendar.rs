//! MCP calendar tools (#441).
//!
//! Three read tools (`list_calendars`, `get_events`,
//! `get_availability` — default **on**) and two write tools
//! (`create_event`, `rsvp_event` — default **off**).
//!
//! Reads serve from the local event cache (with the same
//! recurrence expansion the in-app CalendarView uses); the
//! free/busy tool additionally queries the server.  Writes mirror
//! the in-app flows: `create_event` follows the
//! `create_calendar_event` command (fresh UID → ORGANIZER
//! resolution → `build_ics` → CalDAV PUT with `If-None-Match: *`
//! → cache upsert), `rsvp_event` follows the surgical-PARTSTAT
//! `respond_to_invite` path for events already on the user's
//! calendar.
//!
//! ## Outbound-mail side effects
//!
//! Both write tools make the *server* send mail: a PUT with
//! ATTENDEE lines makes Nextcloud's scheduling plugin dispatch
//! iMIP invitation emails, and a PARTSTAT change dispatches the
//! REPLY to the organiser.  The tool descriptions say so — that
//! text is also what the AI settings page shows next to the
//! toggles.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{Value, json};
use unkai_core::UnkaiError;
use unkai_core::models::{CalendarEvent, EventAttendee, NextcloudAccount};
use unkai_store::Cache;
use unkai_store::cache::CalendarEventRow;

use crate::nc::{
    account_has_feature, load_nc_accounts, nc_password, resolve_nc_account, resolve_organizer,
};
use crate::registry::{NextcloudFeature, ToolAccess, ToolContext, ToolDescriptor, ToolRegistry};
use crate::util::{
    internal, invalid, json_result, optional_str, optional_str_list, required_datetime,
    required_str, required_str_list, schema,
};

/// Cap on `get_events` output — agents work in bounded context
/// windows and a year of a busy calendar can be thousands of rows.
const MAX_EVENTS: usize = 500;

pub(crate) fn register_calendar_tools(registry: &mut ToolRegistry) {
    registry.register(
        ToolDescriptor {
            id: "list_calendars",
            category: "calendar",
            access: ToolAccess::Read,
            requires: Some(NextcloudFeature::Calendar),
            description:
                "List the user's calendars as Unkai Mail has them synced. Each entry carries \
                 the calendar id that get_events and create_event take, plus a read_only flag \
                 — events can only be created in calendars where read_only is false.",
        },
        schema(json!({"type": "object", "properties": {}})),
        Arc::new(|ctx, args| Box::pin(list_calendars(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "get_events",
            category: "calendar",
            access: ToolAccess::Read,
            requires: Some(NextcloudFeature::Calendar),
            description:
                "List cached calendar events in a time range, recurring events expanded into \
                 concrete occurrences, sorted by start. Defaults to every visible calendar; \
                 pass calendar_ids to narrow. The returned event_id is what rsvp_event takes \
                 (occurrences of a recurring series share their series' id).",
        },
        schema(json!({
            "type": "object",
            "required": ["range_start", "range_end"],
            "properties": {
                "range_start": {
                    "type": "string",
                    "description": "Start of the window, RFC 3339 (e.g. 2026-08-01T00:00:00Z)."
                },
                "range_end": {
                    "type": "string",
                    "description": "End of the window (exclusive), RFC 3339."
                },
                "calendar_ids": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Restrict to these calendars (see list_calendars). Omit for all visible calendars."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(get_events(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "get_availability",
            category: "calendar",
            access: ToolAccess::Read,
            requires: Some(NextcloudFeature::Calendar),
            description:
                "Check when people are busy in a time range, for scheduling. Attendees on the \
                 same Nextcloud are answered via CalDAV free/busy; everyone else falls back \
                 to events in the user's own calendars that list them as attendee. source \
                 tells which of the two answered ('unknown' means no signal, not 'free').",
        },
        schema(json!({
            "type": "object",
            "required": ["attendee_emails", "range_start", "range_end"],
            "properties": {
                "attendee_emails": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "description": "Email addresses of the people to check."
                },
                "range_start": {"type": "string", "description": "RFC 3339 start of the window."},
                "range_end": {"type": "string", "description": "RFC 3339 end of the window."},
                "nextcloud_account_id": {
                    "type": "string",
                    "description": "Which connected source to ask. Only needed when several offer calendars."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(get_availability(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "create_event",
            category: "calendar",
            access: ToolAccess::Write,
            requires: Some(NextcloudFeature::Calendar),
            description:
                "Create a calendar event. IMPORTANT: when attendees are listed, the user's \
                 Nextcloud server immediately emails them iMIP calendar invitations on save — \
                 this tool sends real invitations, not a draft. Writes are refused for \
                 calendars marked read_only in list_calendars.",
        },
        schema(json!({
            "type": "object",
            "required": ["calendar_id", "summary", "start", "end"],
            "properties": {
                "calendar_id": {
                    "type": "string",
                    "description": "Target calendar (see list_calendars; must not be read_only)."
                },
                "summary": {"type": "string", "description": "Event title."},
                "start": {"type": "string", "description": "RFC 3339 start time."},
                "end": {"type": "string", "description": "RFC 3339 end time."},
                "all_day": {
                    "type": "boolean",
                    "description": "All-day event: the date parts of start/end define the covered days."
                },
                "description": {"type": "string"},
                "location": {"type": "string"},
                "attendees": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Attendee email addresses. Each will receive an iMIP invitation email from the server."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(create_event(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "rsvp_event",
            category: "calendar",
            access: ToolAccess::Write,
            requires: Some(NextcloudFeature::Calendar),
            description:
                "Answer a meeting invitation that is already on the user's calendar: accept, \
                 decline, or mark tentative. The user's server notifies the organiser by \
                 email (iMIP REPLY). Takes the event_id from get_events.",
        },
        schema(json!({
            "type": "object",
            "required": ["event_id", "response"],
            "properties": {
                "event_id": {
                    "type": "string",
                    "description": "The event to respond to (see get_events)."
                },
                "response": {
                    "type": "string",
                    "enum": ["accepted", "declined", "tentative"],
                    "description": "The user's answer."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(rsvp_event(ctx, args))),
    );
}

// ── Shared helpers ──────────────────────────────────────────────

/// Expand cached events into concrete occurrences for a window —
/// mirror of the Tauri layer's `expand_calendar_events_in_range`,
/// so agents see exactly what the in-app CalendarView shows.
pub(crate) fn expand_events_in_range(
    cache: &Cache,
    calendar_ids: &[String],
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> Result<Vec<CalendarEvent>, ErrorData> {
    let input = cache
        .list_events_for_expansion(calendar_ids, range_start, range_end)
        .map_err(|e| internal(format!("cache read failed: {e}")))?;

    let mut overrides_by_master: HashMap<&str, Vec<&CalendarEvent>> = HashMap::new();
    for ov in &input.overrides {
        if let Some(master_id) = ov.id.rsplit_once("::").map(|(prefix, _)| prefix) {
            overrides_by_master.entry(master_id).or_default().push(ov);
        }
    }

    let mut out: Vec<CalendarEvent> = input.singletons;
    for master in &input.masters {
        let ovs = overrides_by_master
            .get(master.id.as_str())
            .cloned()
            .unwrap_or_default();
        out.extend(unkai_caldav::expand_event(
            master,
            &ovs,
            range_start,
            range_end,
        ));
    }
    out.sort_by_key(|e| e.start);
    Ok(out)
}

fn require_range(args: &Option<JsonObject>) -> Result<(DateTime<Utc>, DateTime<Utc>), ErrorData> {
    let start = required_datetime(args, "range_start")?;
    let end = required_datetime(args, "range_end")?;
    if end <= start {
        return Err(invalid("range_end must be after range_start"));
    }
    Ok((start, end))
}

fn find_nc_account(ctx: &ToolContext, nc_id: &str) -> Result<NextcloudAccount, ErrorData> {
    load_nc_accounts(&ctx.cache)
        .into_iter()
        .find(|a| a.id == nc_id)
        .ok_or_else(|| {
            internal(format!(
                "the connected source '{nc_id}' referenced by the cache no longer exists"
            ))
        })
}

/// Convert a `CalendarEvent` (post-write) into the row shape the
/// cache expects — mirror of the Tauri layer's helper of the same
/// purpose, so MCP writes land in the cache exactly like in-app
/// writes do.
fn event_to_row(event: &CalendarEvent, href: &str, etag: &str, ics_raw: &str) -> CalendarEventRow {
    CalendarEventRow {
        uid: event.id.clone(),
        recurrence_id: event.recurrence_id,
        href: href.to_string(),
        etag: etag.to_string(),
        summary: event.summary.clone(),
        description: event.description.clone(),
        start: event.start,
        end: event.end,
        location: event.location.clone(),
        rrule: event.rrule.clone(),
        rdate: event.rdate.clone(),
        exdate: event.exdate.clone(),
        url: event.url.clone(),
        transparency: event.transparency.clone(),
        attendees: event.attendees.clone(),
        reminders: event.reminders.clone(),
        latitude: event.latitude,
        longitude: event.longitude,
        ics_raw: ics_raw.to_string(),
    }
}

fn event_json(event: &CalendarEvent) -> Value {
    json!({
        "event_id": event.id,
        "summary": event.summary,
        "description": event.description,
        "start": event.start.to_rfc3339(),
        "end": event.end.to_rfc3339(),
        "location": event.location,
        "url": event.url,
        "transparency": event.transparency,
        "recurring": event.rrule.is_some(),
        "attendees": event.attendees.iter().map(|a| json!({
            "email": a.email,
            "name": a.common_name,
            "status": a.status,
        })).collect::<Vec<_>>(),
    })
}

// ── Read handlers ───────────────────────────────────────────────

async fn list_calendars(
    ctx: ToolContext,
    _args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let accounts = load_nc_accounts(&ctx.cache);
    let mut calendars: Vec<Value> = Vec::new();
    for account in accounts
        .iter()
        .filter(|a| account_has_feature(a, NextcloudFeature::Calendar))
    {
        let rows = ctx
            .cache
            .list_calendars(&account.id)
            .map_err(|e| internal(format!("cache read failed: {e}")))?;
        calendars.extend(rows.iter().map(|c| {
            json!({
                "id": c.id,
                "name": c.display_name,
                "color": c.color,
                "read_only": c.read_only,
                "hidden": c.hidden,
                "nextcloud_account_id": c.nextcloud_account_id,
            })
        }));
    }
    Ok(json_result(json!({"calendars": calendars})))
}

async fn get_events(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let (range_start, range_end) = require_range(&args)?;
    let requested = optional_str_list(&args, "calendar_ids")?;

    let calendar_ids: Vec<String> = if requested.is_empty() {
        // Default scope: what the user sees — every synced,
        // non-hidden calendar across all calendar-capable sources.
        let accounts = load_nc_accounts(&ctx.cache);
        let mut ids = Vec::new();
        for account in accounts
            .iter()
            .filter(|a| account_has_feature(a, NextcloudFeature::Calendar))
        {
            let rows = ctx
                .cache
                .list_calendars(&account.id)
                .map_err(|e| internal(format!("cache read failed: {e}")))?;
            ids.extend(rows.into_iter().filter(|c| !c.hidden).map(|c| c.id));
        }
        ids
    } else {
        requested
    };

    let events = expand_events_in_range(&ctx.cache, &calendar_ids, range_start, range_end)?;
    let truncated = events.len() > MAX_EVENTS;
    let events: Vec<Value> = events.iter().take(MAX_EVENTS).map(event_json).collect();

    Ok(json_result(json!({
        "result_count": events.len(),
        "truncated": truncated,
        "events": events,
    })))
}

async fn get_availability(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let attendee_emails = required_str_list(&args, "attendee_emails")?;
    let (range_start, range_end) = require_range(&args)?;
    let account = resolve_nc_account(&ctx, &args, NextcloudFeature::Calendar)?;

    // Free/busy + the user-by-email lookup are Nextcloud OCS /
    // scheduling features; other sources degrade to the
    // local-cache scan and never see the empty password.
    let app_password = if account.is_nextcloud() {
        nc_password(&account)?
    } else {
        String::new()
    };

    // Pre-load the local events once so the per-attendee scan
    // doesn't repeat the SQL + expansion work.
    let calendar_ids: Vec<String> = ctx
        .cache
        .list_calendars(&account.id)
        .map_err(|e| internal(format!("cache read failed: {e}")))?
        .into_iter()
        .map(|c| c.id)
        .collect();
    let local_events = expand_events_in_range(&ctx.cache, &calendar_ids, range_start, range_end)?;

    let mut out: Vec<Value> = Vec::with_capacity(attendee_emails.len());
    for email in attendee_emails {
        let lower = email.trim().to_ascii_lowercase();
        if lower.is_empty() {
            continue;
        }

        // Step 1: is this an account on the user's Nextcloud?
        // Soft-fail so one bad lookup doesn't blank the answer.
        let nc_match = if !account.is_nextcloud() {
            None
        } else {
            match unkai_nextcloud::find_user_by_email(
                &account.server_url,
                &account.username,
                &app_password,
                &email,
                &account.trusted_certs,
            )
            .await
            {
                Ok(m) => m,
                Err(e) => {
                    tracing::info!("MCP: sharees lookup for '{email}' failed: {e}");
                    None
                }
            }
        };

        // Events in the user's own calendars that list this person
        // — the fallback signal, and a title source for free/busy
        // periods (which carry none by design).
        let local_for_attendee: Vec<(DateTime<Utc>, DateTime<Utc>, Option<String>)> = local_events
            .iter()
            .filter(|ev| {
                ev.attendees
                    .iter()
                    .any(|a| a.email.to_ascii_lowercase() == lower)
            })
            .map(|ev| {
                (
                    ev.start,
                    ev.end,
                    (!ev.summary.trim().is_empty()).then(|| ev.summary.clone()),
                )
            })
            .collect();

        // Step 2: CalDAV free-busy against the matched principal.
        if let Some(m) = nc_match.as_ref() {
            let principal_url = unkai_caldav::nc_principal_home(&account.server_url, &m.user_id);
            match unkai_caldav::query_free_busy(
                &principal_url,
                &account.username,
                &app_password,
                range_start,
                range_end,
                &account.trusted_certs,
            )
            .await
            {
                Ok(periods) => {
                    out.push(json!({
                        "email": email,
                        "display_name": m.display_name,
                        "source": "nc-freebusy",
                        "busy_periods": periods.iter().map(|p| json!({
                            "start": p.start.to_rfc3339(),
                            "end": p.end.to_rfc3339(),
                            "kind": busy_kind(&p.kind),
                            "summary": local_for_attendee
                                .iter()
                                .find(|(s, _, _)| *s == p.start)
                                .and_then(|(_, _, sum)| sum.clone()),
                        })).collect::<Vec<_>>(),
                    }));
                    continue;
                }
                Err(e) => {
                    // Common: their calendar isn't shared with us,
                    // the REPORT 403s.  Fall to the local scan.
                    tracing::info!(
                        "MCP: free-busy unavailable for {} ({email}): {e}",
                        m.user_id
                    );
                }
            }
        }

        // Step 3: local-cache fallback.
        let busy: Vec<Value> = local_for_attendee
            .iter()
            .map(|(start, end, summary)| {
                json!({
                    "start": start.to_rfc3339(),
                    "end": end.to_rfc3339(),
                    "kind": "busy",
                    "summary": summary,
                })
            })
            .collect();
        let source = if !busy.is_empty() {
            "local-cache"
        } else if nc_match.is_some() {
            "unknown"
        } else {
            "local-cache"
        };
        out.push(json!({
            "email": email,
            "display_name": nc_match.as_ref().map(|m| m.display_name.clone()),
            "source": source,
            "busy_periods": busy,
        }));
    }

    Ok(json_result(json!({"attendees": out})))
}

fn busy_kind(kind: &unkai_caldav::BusyKind) -> &'static str {
    match kind {
        unkai_caldav::BusyKind::Busy => "busy",
        unkai_caldav::BusyKind::Tentative => "tentative",
        unkai_caldav::BusyKind::Unavailable => "unavailable",
        unkai_caldav::BusyKind::Free => "free",
    }
}

// ── Write path ──────────────────────────────────────────────────

/// Everything `create_event` needs beyond the target calendar.
pub(crate) struct EventSpec {
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub url: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    /// Attendee email addresses.  Non-empty means the server WILL
    /// send iMIP invitation mails on the PUT.
    pub attendees: Vec<String>,
}

/// A successfully created event, with everything needed to mirror
/// it into the cache — or roll it back (#441 composite).
pub(crate) struct CreatedEvent {
    pub event_id: String,
    pub href: String,
    pub etag: String,
    pub ics: String,
    pub calendar_id: String,
    pub nc_account_id: String,
    pub event: CalendarEvent,
}

/// Shared implementation behind the `create_event` tool and the
/// `create_meeting_invite` composite: validate, resolve ORGANIZER,
/// render ICS, PUT.  Deliberately does NOT touch the cache — the
/// caller decides when the event is final (`upsert_created_event`),
/// which is what lets the composite roll back cleanly.
/// Resolve `calendar_id` to its `(nc_account_id, server_path)`
/// and refuse read-only calendars before any network traffic —
/// the flag is the server's own privilege answer from discovery.
/// Shared by `create_event` and the composite's pre-check (which
/// must fail *before* it creates a Talk room it would then have
/// to roll back).
pub(crate) fn validate_writable_calendar(
    ctx: &ToolContext,
    calendar_id: &str,
) -> Result<(String, String), ErrorData> {
    let (nc_id, calendar_path) = ctx
        .cache
        .get_calendar_server_path(calendar_id)
        .map_err(|e| internal(format!("cache read failed: {e}")))?
        .ok_or_else(|| {
            invalid(format!(
                "calendar '{calendar_id}' is not in the local cache — call list_calendars \
                 for valid ids"
            ))
        })?;

    let read_only = ctx
        .cache
        .list_calendars(&nc_id)
        .map_err(|e| internal(format!("cache read failed: {e}")))?
        .iter()
        .find(|c| c.id == calendar_id)
        .is_some_and(|c| c.read_only);
    if read_only {
        return Err(invalid(format!(
            "calendar '{calendar_id}' is read-only — pick a calendar with read_only=false \
             from list_calendars"
        )));
    }
    Ok((nc_id, calendar_path))
}

pub(crate) async fn create_event_impl(
    ctx: &ToolContext,
    calendar_id: &str,
    spec: EventSpec,
) -> Result<CreatedEvent, ErrorData> {
    let (nc_id, calendar_path) = validate_writable_calendar(ctx, calendar_id)?;

    for attendee in &spec.attendees {
        if !attendee.contains('@') {
            return Err(invalid(format!(
                "attendee '{attendee}' is not an email address"
            )));
        }
    }
    if spec.end <= spec.start {
        return Err(invalid("end must be after start"));
    }

    let account = find_nc_account(ctx, &nc_id)?;
    let uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());

    // All-day events cover midnight…23:59:59 of the named days so
    // `build_ics` recognises the all-day shape — same convention
    // as the in-app editor.
    let (start, end) = if spec.all_day {
        let s = Utc.from_utc_datetime(&spec.start.date_naive().and_hms_opt(0, 0, 0).unwrap());
        let e = Utc.from_utc_datetime(&spec.end.date_naive().and_hms_opt(23, 59, 59).unwrap());
        (s, e)
    } else {
        (spec.start, spec.end)
    };

    let event = CalendarEvent {
        id: uid.clone(),
        summary: spec.summary,
        description: spec.description,
        start,
        end,
        location: spec.location,
        rrule: None,
        rdate: vec![],
        exdate: vec![],
        recurrence_id: None,
        url: spec.url,
        transparency: None,
        attendees: spec
            .attendees
            .iter()
            .map(|email| EventAttendee {
                email: email.clone(),
                common_name: None,
                status: None,
                role: None,
                force_send_reply: false,
            })
            .collect(),
        reminders: vec![],
        latitude: None,
        longitude: None,
    };

    // Local sources can't (and needn't) ask an OCS endpoint who
    // the organizer is — fall back to the account-derived line.
    let (organizer_email, organizer_name) = if account.is_local() {
        crate::nc::organizer_local(&account)
    } else {
        let app_password = nc_password(&account)?;
        resolve_organizer(&account, &app_password, !event.attendees.is_empty()).await
    };
    let ics = unkai_caldav::build_ics(&event, Some(&organizer_email), organizer_name.as_deref());

    let outcome = if account.is_local() {
        unkai_caldav::WriteOutcome {
            href: format!("{}/{uid}.ics", calendar_path.trim_end_matches('/')),
            etag: uuid::Uuid::new_v4().to_string(),
        }
    } else {
        let app_password = nc_password(&account)?;
        unkai_caldav::create_event(
            &account.server_url,
            &calendar_path,
            &account.username,
            &app_password,
            &uid,
            &ics,
            &account.trusted_certs,
        )
        .await
        .map_err(|e| {
            // Same permission signal handling as the in-app flow:
            // remember the read-only answer so the next attempt is
            // refused locally (the UI picks the flag up too).
            if matches!(e, UnkaiError::CalDavWriteForbidden(_)) {
                if let Err(flip) = ctx.cache.set_calendar_read_only(calendar_id, true) {
                    tracing::warn!("MCP: could not flag calendar read-only: {flip}");
                }
                invalid(format!(
                    "the server refused the write — calendar '{calendar_id}' is read-only"
                ))
            } else {
                internal(format!("CalDAV create failed: {e}"))
            }
        })?
    };

    Ok(CreatedEvent {
        event_id: format!("{calendar_id}::{uid}"),
        href: outcome.href,
        etag: outcome.etag,
        ics,
        calendar_id: calendar_id.to_string(),
        nc_account_id: nc_id,
        event,
    })
}

/// Mirror a created event into the local cache so the UI shows it
/// without waiting for the next sync round.
pub(crate) fn upsert_created_event(
    ctx: &ToolContext,
    created: &CreatedEvent,
) -> Result<(), ErrorData> {
    let row = event_to_row(&created.event, &created.href, &created.etag, &created.ics);
    ctx.cache
        .upsert_single_event(&created.calendar_id, &row)
        .map_err(|e| internal(format!("cache write failed: {e}")))
}

/// Best-effort server-side removal of a just-created event —
/// the composite's rollback.  Deleting an event with attendees
/// makes the server send iMIP CANCEL notices, which is exactly
/// what should follow an invite that was sent by the failed step.
pub(crate) async fn rollback_created_event(
    ctx: &ToolContext,
    created: &CreatedEvent,
) -> Result<(), String> {
    let account =
        find_nc_account(ctx, &created.nc_account_id).map_err(|e| e.message.to_string())?;
    if account.is_local() {
        return Ok(());
    }
    let app_password = nc_password(&account).map_err(|e| e.message.to_string())?;
    unkai_caldav::delete_event(
        &created.href,
        &account.username,
        &app_password,
        &created.etag,
        &account.trusted_certs,
    )
    .await
    .map_err(|e| e.to_string())
}

async fn create_event(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let calendar_id = required_str(&args, "calendar_id")?;
    let spec = EventSpec {
        summary: required_str(&args, "summary")?,
        description: optional_str(&args, "description")?,
        location: optional_str(&args, "location")?,
        url: None,
        start: required_datetime(&args, "start")?,
        end: required_datetime(&args, "end")?,
        all_day: crate::util::optional_bool(&args, "all_day")?.unwrap_or(false),
        attendees: optional_str_list(&args, "attendees")?,
    };
    let had_attendees = !spec.attendees.is_empty();

    let created = create_event_impl(&ctx, &calendar_id, spec).await?;
    upsert_created_event(&ctx, &created)?;

    let note = if had_attendees {
        "The event was created and the server is sending iMIP invitation emails to the attendees."
    } else {
        "The event was created. No attendees were listed, so no invitations were sent."
    };
    Ok(json_result(json!({
        "status": "event_created",
        "event_id": created.event_id,
        "calendar_id": created.calendar_id,
        "note": note,
    })))
}

async fn rsvp_event(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let event_id = required_str(&args, "event_id")?;
    let partstat = match required_str(&args, "response")?
        .to_ascii_lowercase()
        .as_str()
    {
        "accepted" => "ACCEPTED",
        "declined" => "DECLINED",
        "tentative" => "TENTATIVE",
        other => {
            return Err(invalid(format!(
                "response must be 'accepted', 'declined', or 'tentative' (got '{other}')"
            )));
        }
    };

    let handle = ctx
        .cache
        .get_event_server_handle(&event_id)
        .map_err(|e| internal(format!("cache read failed: {e}")))?
        .ok_or_else(|| {
            invalid("event not found in the local cache — use an event_id from get_events")
        })?;
    let account = find_nc_account(&ctx, &handle.nextcloud_account_id)?;
    let app_password = if account.is_local() {
        String::new()
    } else {
        nc_password(&account)?
    };

    // Candidate identities for "which ATTENDEE row is the user",
    // in priority order — NC profile email (the address Sabre's
    // iTIP broker keys on), then every configured mail-account
    // address, then the synthesised local fallback.  We answer as
    // whichever of these is actually in the invite's ATTENDEE
    // list, so the server pairs the PARTSTAT change with the
    // user's principal and dispatches the REPLY iMIP.
    let mut candidates: Vec<String> = Vec::new();
    let nc_profile_email = if account.is_nextcloud() {
        match unkai_nextcloud::fetch_current_user(
            &account.server_url,
            &account.username,
            &app_password,
            &account.trusted_certs,
        )
        .await
        {
            Ok(p) => p.email,
            Err(e) => {
                tracing::warn!("MCP RSVP: NC user-profile lookup failed ({e})");
                None
            }
        }
    } else {
        None
    };
    if let Some(e) = nc_profile_email.as_deref() {
        candidates.push(e.to_string());
    }
    for a in crate::util::load_accounts(&ctx)? {
        candidates.push(a.email);
    }
    candidates.push(crate::nc::organizer_local(&account).0);
    let mut seen = std::collections::HashSet::new();
    let candidates: Vec<String> = candidates
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .filter(|s| seen.insert(s.to_ascii_lowercase()))
        .collect();

    let events = unkai_caldav::parse_ics(&handle.ics_raw)
        .map_err(|e| internal(format!("could not parse the cached event: {e}")))?;
    let mut event = events
        .into_iter()
        .next()
        .ok_or_else(|| internal("cached event body has no VEVENT"))?;

    let attendee_email = {
        let inbound: std::collections::HashSet<String> = event
            .attendees
            .iter()
            .map(|a| a.email.to_ascii_lowercase())
            .collect();
        candidates
            .iter()
            .find(|c| inbound.contains(&c.to_ascii_lowercase()))
            .cloned()
            .or(nc_profile_email)
            .or_else(|| candidates.into_iter().next())
            .unwrap_or_else(|| crate::nc::organizer_local(&account).0)
    };

    // Mirror the change onto the parsed event for the cache row.
    let mut matched = false;
    for att in event.attendees.iter_mut() {
        if att.email.eq_ignore_ascii_case(attendee_email.trim()) {
            att.status = Some(partstat.to_string());
            att.force_send_reply = true;
            matched = true;
        }
    }
    if !matched {
        event.attendees.push(EventAttendee {
            email: attendee_email.trim().to_string(),
            common_name: None,
            status: Some(partstat.to_string()),
            role: Some("REQ-PARTICIPANT".into()),
            force_send_reply: true,
        });
    }
    // TENTATIVE renders visually distinct (free slot); ACCEPTED /
    // DECLINED block the slot — same convention as the in-app RSVP.
    event.transparency = Some(if partstat == "TENTATIVE" {
        "TRANSPARENT".into()
    } else {
        "OPAQUE".into()
    });

    // Surgical edit: replace only the user's PARTSTAT (plus
    // SCHEDULE-FORCE-SEND=REPLY) and keep every other byte — the
    // server only dispatches the REPLY iMIP when the diff against
    // its stored copy is exactly that clean.
    let surgical =
        unkai_caldav::ical::surgical_set_partstat(&handle.ics_raw, &attendee_email, partstat, true);

    let outcome = if account.is_local() {
        unkai_caldav::WriteOutcome {
            href: handle.href.clone(),
            etag: uuid::Uuid::new_v4().to_string(),
        }
    } else {
        unkai_caldav::update_event(
            &handle.href,
            &account.username,
            &app_password,
            &handle.etag,
            &surgical,
            &account.trusted_certs,
        )
        .await
        .map_err(|e| match e {
            UnkaiError::EtagMismatch(_) => invalid(
                "the event changed on the server since Unkai Mail last synced — open the \
                 calendar in Unkai Mail to refresh, then retry",
            ),
            UnkaiError::CalDavWriteForbidden(_) => {
                invalid("the server refused the write — this calendar is read-only")
            }
            other => internal(format!("CalDAV update failed: {other}")),
        })?
    };

    let row = event_to_row(&event, &outcome.href, &outcome.etag, &surgical);
    ctx.cache
        .upsert_single_event(&handle.calendar_id, &row)
        .map_err(|e| internal(format!("cache write failed: {e}")))?;
    ctx.cache
        .upsert_rsvp_response(&handle.uid, partstat)
        .map_err(|e| internal(format!("cache write failed: {e}")))?;

    let note = if account.is_local() {
        "The response was recorded locally. This calendar has no server, so no reply email \
         was sent to the organiser."
    } else {
        "The response was saved to the calendar; the server notifies the organiser by email."
    };
    Ok(json_result(json!({
        "status": "rsvp_recorded",
        "event_id": event_id,
        "response": partstat,
        "responded_as": attendee_email,
        "note": note,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nc::test_support::{caps, nc_account};
    use crate::testutil::{invoke, mail_account, result_payload, test_context};
    use serde_json::json;
    use unkai_core::models::DavSourceKind;
    use unkai_store::cache::CalendarRow;
    use unkai_store::{account_store, nextcloud_store};

    const CAL_PATH: &str = "local://acc/cal";

    /// Seed a connected local source with one calendar; returns
    /// the calendar's cache id.
    fn seed_calendar(ctx: &ToolContext, read_only: bool) -> String {
        nextcloud_store::upsert_account(
            &ctx.cache,
            nc_account("acc", DavSourceKind::Local, Some(caps(false, true, true))),
        )
        .unwrap();
        ctx.cache
            .upsert_calendars(
                "acc",
                &[CalendarRow {
                    path: CAL_PATH.into(),
                    display_name: "Personal".into(),
                    color: Some("#2bb0ed".into()),
                    ctag: None,
                    hidden: false,
                    muted: false,
                    read_only,
                }],
            )
            .unwrap();
        format!("acc::{CAL_PATH}")
    }

    fn seed_event(ctx: &ToolContext, calendar_id: &str, uid: &str, attendee: &str) {
        let start = Utc.with_ymd_and_hms(2026, 8, 1, 9, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        let ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             DTSTART:20260801T090000Z\r\nDTEND:20260801T100000Z\r\nSUMMARY:Standup\r\n\
             ATTENDEE;CN=Alex;PARTSTAT=NEEDS-ACTION;ROLE=REQ-PARTICIPANT:mailto:{attendee}\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let row = CalendarEventRow {
            uid: uid.into(),
            recurrence_id: None,
            href: format!("{CAL_PATH}/{uid}.ics"),
            etag: "etag-1".into(),
            summary: "Standup".into(),
            description: None,
            start,
            end,
            location: None,
            rrule: None,
            rdate: vec![],
            exdate: vec![],
            url: None,
            transparency: None,
            attendees: vec![EventAttendee {
                email: attendee.into(),
                common_name: Some("Alex".into()),
                status: Some("NEEDS-ACTION".into()),
                role: Some("REQ-PARTICIPANT".into()),
                force_send_reply: false,
            }],
            reminders: vec![],
            latitude: None,
            longitude: None,
            ics_raw: ics,
        };
        ctx.cache.upsert_single_event(calendar_id, &row).unwrap();
    }

    #[tokio::test]
    async fn list_calendars_exposes_the_read_only_flag() {
        let ctx = test_context();
        seed_calendar(&ctx, true);
        let result = invoke(&ctx, "list_calendars", json!({})).await.unwrap();
        let payload = result_payload(&result);
        let calendars = payload["calendars"].as_array().unwrap();
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0]["name"], "Personal");
        assert_eq!(calendars[0]["read_only"], true);
        assert_eq!(calendars[0]["nextcloud_account_id"], "acc");
    }

    #[tokio::test]
    async fn get_events_returns_cached_events_in_range() {
        let ctx = test_context();
        let calendar_id = seed_calendar(&ctx, false);
        seed_event(&ctx, &calendar_id, "evt-1", "alex@example.com");

        let result = invoke(
            &ctx,
            "get_events",
            json!({
                "range_start": "2026-08-01T00:00:00Z",
                "range_end": "2026-08-02T00:00:00Z",
            }),
        )
        .await
        .unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["result_count"], 1);
        assert_eq!(payload["truncated"], false);
        let event = &payload["events"][0];
        assert_eq!(event["summary"], "Standup");
        assert_eq!(event["event_id"], format!("{calendar_id}::evt-1"));
        assert_eq!(event["attendees"][0]["email"], "alex@example.com");

        // Outside the window: nothing.
        let result = invoke(
            &ctx,
            "get_events",
            json!({
                "range_start": "2026-09-01T00:00:00Z",
                "range_end": "2026-09-02T00:00:00Z",
            }),
        )
        .await
        .unwrap();
        assert_eq!(result_payload(&result)["result_count"], 0);
    }

    #[tokio::test]
    async fn get_events_rejects_inverted_ranges() {
        let ctx = test_context();
        let err = invoke(
            &ctx,
            "get_events",
            json!({
                "range_start": "2026-08-02T00:00:00Z",
                "range_end": "2026-08-01T00:00:00Z",
            }),
        )
        .await
        .expect_err("inverted range should error");
        assert!(err.message.contains("range_end"));
    }

    #[tokio::test]
    async fn create_event_rejects_read_only_calendars_before_any_network() {
        let ctx = test_context();
        let calendar_id = seed_calendar(&ctx, true);
        let err = invoke(
            &ctx,
            "create_event",
            json!({
                "calendar_id": calendar_id,
                "summary": "Planning",
                "start": "2026-08-01T09:00:00Z",
                "end": "2026-08-01T10:00:00Z",
            }),
        )
        .await
        .expect_err("read-only calendar should refuse writes");
        assert!(err.message.contains("read-only"));
    }

    #[tokio::test]
    async fn create_event_rejects_unknown_calendars_and_bad_attendees() {
        let ctx = test_context();
        let err = invoke(
            &ctx,
            "create_event",
            json!({
                "calendar_id": "nope::x",
                "summary": "Planning",
                "start": "2026-08-01T09:00:00Z",
                "end": "2026-08-01T10:00:00Z",
            }),
        )
        .await
        .expect_err("unknown calendar should error");
        assert!(err.message.contains("list_calendars"));

        let calendar_id = seed_calendar(&ctx, false);
        let err = invoke(
            &ctx,
            "create_event",
            json!({
                "calendar_id": calendar_id,
                "summary": "Planning",
                "start": "2026-08-01T09:00:00Z",
                "end": "2026-08-01T10:00:00Z",
                "attendees": ["not-an-email"],
            }),
        )
        .await
        .expect_err("non-email attendee should error");
        assert!(err.message.contains("not-an-email"));
    }

    #[tokio::test]
    async fn create_event_on_a_local_calendar_lands_in_the_cache() {
        let ctx = test_context();
        let calendar_id = seed_calendar(&ctx, false);
        let result = invoke(
            &ctx,
            "create_event",
            json!({
                "calendar_id": calendar_id,
                "summary": "Planning",
                "start": "2026-08-03T09:00:00Z",
                "end": "2026-08-03T10:00:00Z",
            }),
        )
        .await
        .unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["status"], "event_created");
        assert!(payload["note"].as_str().unwrap().contains("No attendees"));

        let events = expand_events_in_range(
            &ctx.cache,
            &[calendar_id],
            Utc.with_ymd_and_hms(2026, 8, 3, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 4, 0, 0, 0).unwrap(),
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "Planning");
    }

    #[tokio::test]
    async fn rsvp_records_the_answer_for_a_local_calendar() {
        let ctx = test_context();
        let calendar_id = seed_calendar(&ctx, false);
        seed_event(&ctx, &calendar_id, "evt-1", "alex@example.com");
        // The identity resolution matches the invite's ATTENDEE row
        // against the user's configured mail-account addresses.
        account_store::add_account(&ctx.cache, mail_account("mail-1")).unwrap();

        let event_id = format!("{calendar_id}::evt-1");
        let result = invoke(
            &ctx,
            "rsvp_event",
            json!({"event_id": event_id, "response": "accepted"}),
        )
        .await
        .unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["status"], "rsvp_recorded");
        assert_eq!(payload["response"], "ACCEPTED");
        assert_eq!(payload["responded_as"], "alex@example.com");
        // Local source: the note must say no reply email went out.
        assert!(payload["note"].as_str().unwrap().contains("no reply email"));

        // The RSVP bookkeeping the invite cards read is updated…
        assert_eq!(
            ctx.cache.get_rsvp_response("evt-1").unwrap().as_deref(),
            Some("ACCEPTED")
        );
        // …and the cached body carries the surgical PARTSTAT edit.
        let handle = ctx
            .cache
            .get_event_server_handle(&format!("{calendar_id}::evt-1"))
            .unwrap()
            .unwrap();
        assert!(handle.ics_raw.contains("PARTSTAT=ACCEPTED"));
    }

    #[tokio::test]
    async fn rsvp_rejects_unknown_events_and_answers() {
        let ctx = test_context();
        let err = invoke(
            &ctx,
            "rsvp_event",
            json!({"event_id": "nope::x::y", "response": "accepted"}),
        )
        .await
        .expect_err("unknown event should error");
        assert!(err.message.contains("get_events"));

        let err = invoke(
            &ctx,
            "rsvp_event",
            json!({"event_id": "nope::x::y", "response": "maybe"}),
        )
        .await
        .expect_err("bad response value should error");
        assert!(err.message.contains("tentative"));
    }
}

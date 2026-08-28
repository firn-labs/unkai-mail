//! CalDAV calendars, events, invites, and availability.
//!
//! Mirrors `ui/src/lib/api/calendar.ts`.

use serde::Deserialize;
use serde::Serialize;
use unkai_caldav::BusyKind as CaldavBusyKind;
use unkai_caldav::Calendar as CaldavCalendar;
use unkai_caldav::RawEvent;
use unkai_caldav::build_ics as caldav_build_ics;
use unkai_caldav::create_calendar as caldav_create_calendar;
use unkai_caldav::delete_calendar as caldav_delete_calendar;
use unkai_caldav::delete_event as caldav_delete_event;
use unkai_caldav::list_calendars_at as caldav_list_calendars_at;
use unkai_caldav::nc_principal_home as caldav_nc_principal_home;
use unkai_caldav::probe_calendar_writable as caldav_probe_writable;
use unkai_caldav::query_free_busy as caldav_query_free_busy;
use unkai_caldav::sync_calendar as caldav_sync_calendar;
use unkai_caldav::update_calendar as caldav_update_calendar;
use unkai_caldav::update_event as caldav_update_event;
use unkai_core::UnkaiError;
use unkai_core::models::CalendarEvent;
use unkai_core::models::EventAttendee;
use unkai_core::models::EventReminder;
use unkai_core::models::NextcloudAccount;
use unkai_store::Cache;
use unkai_store::account_store;
use unkai_store::cache::CalendarEventRow;
use unkai_store::cache::CalendarEventServerHandle;
use unkai_store::cache::CalendarRow;
use unkai_store::credentials;

use crate::notify::CalendarsUpdatedPayload;
use crate::notify::UiNotifier;
use crate::state::{EventReminderState, SharedSettings};
use crate::support::{
    SyncStatus, caldav_home_of, connect_imap, dav_create_event_for, dav_update_event_for,
    load_account, load_nextcloud_account, url_origin, uses_jmap,
};

/// Surgical PARTSTAT update for an event already in the user's
/// cache — the EventEditor's RSVP dropdown lands here when an
/// attendee changes their response on a meeting that's already
/// on the calendar.
///
/// Why we don't just route this through `update_calendar_event`:
/// regenerating the VEVENT body from form fields drops X-* lines
/// and re-orders properties, which Sabre's iTIP broker reads as
/// a "noisy" diff and silently suppresses the REPLY iMIP.  The
/// inbox card's `respond_to_invite` already implements the
/// byte-preserving surgical path; this command is a thin wrapper
/// that pulls the cached `ics_raw` for an existing event id and
/// hands it to `respond_to_invite` so the same flow applies.
pub async fn rsvp_existing_event(
    event_id: String,
    partstat: String,
    attendee_hint: Option<String>,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let handle = load_event_handle(cache, &event_id)?;
    let calendar_id = handle.calendar_id.clone();
    let raw_ics = handle.ics_raw.clone();
    respond_to_invite(calendar_id, raw_ics, partstat, attendee_hint, cache).await
}

pub fn get_calendars_sync_status(nc_id: String, cache: &Cache) -> Result<SyncStatus, UnkaiError> {
    let last = cache
        .latest_calendar_sync_at(&nc_id)
        .map_err(UnkaiError::from)?
        .map(|t| t.to_rfc3339());
    let count = cache
        .list_calendars(&nc_id)
        .map(|cs| cs.len() as u32)
        .unwrap_or(0);
    Ok(SyncStatus {
        last_synced_at: last,
        count,
    })
}

// ── CalDAV calendars ────────────────────────────────────────────
//
// Calendar sync mirrors the CardDAV flow: one user-facing entry
// point (`sync_nextcloud_calendars`) walks the user's calendars and
// runs an incremental sync-collection REPORT per calendar, persisting
// each delta transactionally via the store. The UI reads cached data
// via `get_cached_calendars` (list for settings / sidebar header) and
// `get_cached_events` (events in a date window — the sidebar body).
//
// What the UI never sees: hrefs, etags, sync tokens, raw ICS blobs.
// Those all stay behind the store boundary.

/// Thin summary of a calendar — what the Svelte side needs to render
/// a row or colour-chip. Sourced from `CachedCalendar` but omits the
/// sync bookkeeping (tokens, ctag) the UI shouldn't care about.
#[derive(Debug, Clone, Serialize)]
pub struct CalendarSummary {
    pub id: String,
    pub nextcloud_account_id: String,
    pub display_name: String,
    pub color: Option<String>,
    pub last_synced_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Layer 1 (Settings). `true` removes the calendar from the sidebar
    /// entirely. Toggled from NextcloudSettings' per-calendar checkboxes.
    #[serde(default)]
    pub hidden: bool,
    /// Layer 2 (sidebar swatch). `true` keeps the calendar in the sidebar
    /// but stops its events from painting on the agenda grid. Toggled via
    /// the coloured swatch in the CalendarView sidebar.
    #[serde(default)]
    pub muted: bool,
    /// CalDAV-derived read-only flag (#236).  Mirrors
    /// `current-user-privilege-set`: `true` when the user can't add
    /// or modify events on this calendar (typical for shared
    /// calendars where the owner granted view-only access).  The
    /// EventEditor hides Delete and removes the calendar from the
    /// new-event picker when this is set.
    #[serde(default)]
    pub read_only: bool,
}

/// Summary returned to the UI after a calendar sync run.
///
/// Per-calendar counts let the UI say "Personal: 4 new, 0 removed"
/// instead of a generic "done". `errors` accumulates per-calendar
/// failures so one broken calendar (commonly a subscribed read-only
/// feed that doesn't support sync-collection) doesn't paint the
/// whole run red.
#[derive(Debug, Clone, Serialize)]
pub struct SyncCalendarsReport {
    pub nc_account_id: String,
    pub calendars_synced: u32,
    pub upserted: u32,
    pub deleted: u32,
    pub errors: Vec<String>,
}

/// Fresh PROPFIND list of the user's calendars on the server.
///
/// Lighter than `sync_nextcloud_calendars` — no per-calendar sync,
/// no cache write. Used in settings UIs where the user just wants
/// to see what calendars exist server-side before toggling sync on.
pub async fn list_nextcloud_calendars(
    nc_id: String,
    cache: &Cache,
) -> Result<Vec<CalendarSummary>, UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    // Local sources: the cache list *is* the calendar list (#413).
    if account.is_local() {
        return Ok(cache
            .list_calendars(&nc_id)?
            .into_iter()
            .map(|c| CalendarSummary {
                id: c.id,
                nextcloud_account_id: c.nextcloud_account_id,
                display_name: c.display_name,
                color: c.color,
                last_synced_at: c.last_synced_at,
                hidden: c.hidden,
                muted: c.muted,
                read_only: c.read_only,
            })
            .collect());
    }
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let calendars: Vec<CaldavCalendar> = caldav_list_calendars_at(
        &caldav_home_of(&account),
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    Ok(calendars
        .into_iter()
        .map(|c| CalendarSummary {
            // Matches the id scheme used by the cache — stable across
            // syncs so the UI can key rows by it whether it's looking
            // at a fresh discovery list or the cached list.
            id: format!("{nc_id}::{}", c.path),
            nextcloud_account_id: nc_id.clone(),
            display_name: c.display_name.unwrap_or(c.name),
            color: c.color,
            // Discovery alone doesn't produce a sync timestamp.
            last_synced_at: None,
            // Raw discovery can't know about local toggles — the
            // cache-backed `get_cached_calendars` path does. This
            // command is only used by the setup probe, so defaulting
            // to fully visible is fine.
            hidden: false,
            muted: false,
            // The discovery path has the privilege-set bit; pass
            // it through so the setup probe can already gray out
            // read-only calendars.
            read_only: c.read_only,
        })
        .collect())
}

/// Pull the latest calendars and events from a Nextcloud account.
///
/// Two phases:
///   1. Discovery (PROPFIND) → `upsert_calendars`. This also prunes
///      any calendar that vanished server-side, cascading its events.
///   2. Per-calendar incremental sync. We pass the previous
///      `sync_token` (from the cache) so the server returns only
///      what changed. A failure on calendar N is logged and added
///      to the report; calendar N+1 still runs.
///
/// Each calendar's delta is committed in its own transaction, so
/// a partial run leaves earlier calendars fully up-to-date.
pub async fn sync_nextcloud_calendars(
    nc_id: String,
    cache: &Cache,
) -> Result<SyncCalendarsReport, UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;

    // A local-only source has nothing to sync with (#413) — the
    // cache *is* the source of truth. Empty report, no error.
    if account.is_local() {
        return Ok(SyncCalendarsReport {
            nc_account_id: nc_id,
            calendars_synced: 0,
            upserted: 0,
            deleted: 0,
            errors: Vec::new(),
        });
    }
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    // ── Phase 1: discovery + reconcile the calendar list ────────
    // Generic DAV sources carry an explicit (RFC 6764-resolved)
    // calendar home; Nextcloud derives it from the server layout.
    let mut server_calendars = caldav_list_calendars_at(
        &caldav_home_of(&account),
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    tracing::info!(
        "CalDAV: {} calendar(s) discovered for {}",
        server_calendars.len(),
        nc_id
    );

    // #236 follow-up — privilege-set parsing alone misses a few
    // Sabre/DAV variants (notably some shared-calendar configs
    // that omit `current-user-privilege-set` entirely or
    // advertise write privileges that the actual PUT then
    // refuses with 404).  OPTIONS is similarly unreliable —
    // some configs return the resource type's full method list
    // regardless of ACL.  The only signal that reliably matches
    // what the user hits at save time is an actual PUT, so we
    // do exactly that: drop a placeholder VEVENT in 1970 (so it
    // never collides with real data), DELETE it on the way out,
    // and treat the PUT verdict as canonical.  The probe fires
    // once per calendar per sync — the cost is one extra
    // request pair on top of the existing PROPFIND/REPORT
    // traffic.
    //
    // We OR with the privilege-set verdict so a writable
    // discovery only stands when both signals agree; any
    // probe failure (network, 5xx) leaves the privilege-set
    // verdict alone rather than misclassify on a transient
    // blip.
    for cal in &mut server_calendars {
        match caldav_probe_writable(
            &cal.path,
            &account.username,
            &app_password,
            &account.trusted_certs,
        )
        .await
        {
            Ok(true) => {
                // PUT succeeded → calendar accepts writes. The
                // privilege-set may still have flagged it
                // read-only (rare); we trust the PUT result and
                // *clear* the flag so a re-shared calendar that
                // gets write access back also resurfaces in the
                // editor.
                if cal.read_only {
                    tracing::info!(
                        "CalDAV: write-probe overrides privilege-set on '{}' \
                         (PUT succeeded → marking writable)",
                        cal.path
                    );
                }
                cal.read_only = false;
            }
            Ok(false) => {
                if !cal.read_only {
                    tracing::info!(
                        "CalDAV: write-probe marks calendar '{}' read-only (PUT 403/404)",
                        cal.path
                    );
                }
                cal.read_only = true;
            }
            Err(e) => {
                tracing::warn!(
                    "CalDAV: write-probe for '{}' failed, keeping privilege-set verdict: {e}",
                    cal.path
                );
            }
        }
    }

    let rows: Vec<CalendarRow> = server_calendars
        .iter()
        .map(|c| CalendarRow {
            path: c.path.clone(),
            display_name: c.display_name.clone().unwrap_or_else(|| c.name.clone()),
            color: c.color.clone(),
            ctag: c.ctag.clone(),
            // Fresh inserts default to fully visible; the `upsert_calendars`
            // ON CONFLICT clause leaves `hidden` and `muted` untouched on
            // updates so existing local toggles survive re-sync.
            hidden: false,
            muted: false,
            // #236 — server-side privilege-set + OPTIONS probe agree
            // on whether the editor lets the user write events here.
            // The upsert refreshes this on every discovery so a calendar
            // that gets re-shared as read-only between syncs flips
            // promptly.
            read_only: c.read_only,
        })
        .collect();
    cache.upsert_calendars(&nc_id, &rows)?;

    // ── Phase 2: sync each calendar individually ────────────────
    let mut report = SyncCalendarsReport {
        nc_account_id: nc_id.clone(),
        calendars_synced: 0,
        upserted: 0,
        deleted: 0,
        errors: Vec::new(),
    };

    for cal in server_calendars {
        // id matches the (nc_id, path) scheme `upsert_calendars`
        // just committed, so `get_calendar_sync_state` and
        // `apply_event_delta` will find/target the right row.
        let cal_id = format!("{nc_id}::{}", cal.path);

        let prev_token = cache
            .get_calendar_sync_state(&cal_id)
            .ok()
            .flatten()
            .and_then(|s| s.sync_token);

        // Origin of the collection itself — see the CardDAV twin for
        // why this isn't `account.server_url` (generic DAV base URLs
        // can carry a path).
        let delta = match caldav_sync_calendar(
            &url_origin(&cal.path),
            &cal.path,
            &account.username,
            &app_password,
            prev_token.as_deref(),
            &account.trusted_certs,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("CalDAV sync failed for calendar '{}': {e}", cal.path);
                report.errors.push(format!("{}: {e}", cal.path));
                continue;
            }
        };

        // One `RawEvent` can carry several VEVENTs (master + overrides
        // at the same href). Flatten into one store row per VEVENT so
        // the range query sees them individually. `ics_raw` is cloned
        // onto every row from the same href — the raw blob stays
        // identical, and the store is optimised for per-row reads,
        // not per-href grouping.
        let upserts: Vec<CalendarEventRow> =
            delta.upserts.iter().flat_map(raw_event_to_rows).collect();

        if let Err(e) = cache.apply_event_delta(
            &cal_id,
            &upserts,
            &delta.deleted_hrefs,
            delta.new_sync_token.as_deref(),
            cal.ctag.as_deref(),
        ) {
            tracing::warn!("apply_event_delta failed for '{}': {e}", cal.path);
            report.errors.push(format!("{}: {e}", cal.path));
            continue;
        }

        report.calendars_synced += 1;
        report.upserted += upserts.len() as u32;
        report.deleted += delta.deleted_hrefs.len() as u32;
    }

    Ok(report)
}

/// Single-calendar sync by app-side `calendar_id`.  Used by the
/// EventEditor to freshen one calendar's events the moment the
/// user opens an event for editing — narrows the window where a
/// stale-etag PUT (the "If-Match failed" race) can happen.  Soft-
/// fails on any error and just propagates it; the caller logs
/// without surfacing a toast because this is best-effort
/// freshening, not a user-initiated sync.
pub async fn sync_calendar_by_id(calendar_id: String, cache: &Cache) -> Result<(), UnkaiError> {
    let (nc_id, path) = cache
        .get_calendar_server_path(&calendar_id)?
        .ok_or_else(|| {
            UnkaiError::Other(format!(
                "calendar '{calendar_id}' is not in the local cache"
            ))
        })?;
    refresh_calendar_cache(cache, &nc_id, &path).await
}

/// Cache-only list of calendars for a Nextcloud account. Used by the
/// sidebar widget on startup so it can paint before the first sync
/// finishes (or if the user is offline).
pub fn get_cached_calendars(
    nc_id: String,
    cache: &Cache,
) -> Result<Vec<CalendarSummary>, UnkaiError> {
    let cached = cache.list_calendars(&nc_id)?;
    Ok(cached
        .into_iter()
        .map(|c| CalendarSummary {
            id: c.id,
            nextcloud_account_id: c.nextcloud_account_id,
            display_name: c.display_name,
            color: c.color,
            last_synced_at: c.last_synced_at,
            hidden: c.hidden,
            muted: c.muted,
            read_only: c.read_only,
        })
        .collect())
}

// ── Calendar management commands (Issue #82) ─────────────────
//
// CalDAV wrappers that add / rename / recolor / delete a calendar
// collection on the server and keep the local cache in step. Each
// mutates exactly one calendar row; the next `sync_nextcloud_
// calendars` run reconciles etag / sync-token / event deltas.
// `set_nextcloud_calendar_hidden` is the only one that doesn't
// talk to the server — hidden is a local-only flag.

/// Create a new calendar on the server and seed a cache row.
///
/// The path segment is a fresh UUID so two concurrent creates can't
/// collide on the wire and so a later rename never rewrites URLs
/// downstream (the slug stays stable regardless of display name).
/// Returns the newly-inserted summary so the UI can add it to the
/// sidebar without a follow-up fetch.
pub async fn create_nextcloud_calendar(
    nc_id: String,
    display_name: String,
    color: Option<String>,
    cache: &Cache,
) -> Result<CalendarSummary, UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;

    let slug = uuid::Uuid::new_v4().to_string();
    let url = if account.is_local() {
        // No server to MKCALENDAR against (#413) — the synthetic
        // path only has to be unique per (account, calendar).
        format!("local://{slug}/")
    } else {
        let app_password = credentials::get_nextcloud_password(&nc_id)?;
        let home = caldav_home_of(&account);
        caldav_create_calendar(
            &home,
            &account.username,
            &app_password,
            &slug,
            &display_name,
            color.as_deref(),
            &account.trusted_certs,
        )
        .await?
    };

    // Seed the cache so the sidebar paints the new calendar
    // instantly. `ctag` / `sync_token` land on the next full sync —
    // no event rows yet anyway, so the bookkeeping gap is cosmetic.
    let row = CalendarRow {
        path: url.clone(),
        display_name: display_name.clone(),
        color: color.clone(),
        ctag: None,
        hidden: false,
        muted: false,
        // The user just created this calendar, so they own it and
        // have full write privileges (#236).  Next discovery cycle
        // confirms via `current-user-privilege-set` PROPFIND.
        read_only: false,
    };
    let id = cache.insert_calendar(&nc_id, &row)?;

    Ok(CalendarSummary {
        id,
        nextcloud_account_id: nc_id,
        display_name,
        color,
        last_synced_at: None,
        hidden: false,
        muted: false,
        // Same reasoning as `insert_calendar` above — fresh
        // user-created calendar is owned by the user, fully
        // writable until next discovery says otherwise.
        read_only: false,
    })
}

/// Rename and/or recolor an existing calendar via a single CalDAV
/// `PROPPATCH`. Either argument may be `None` — passing both `None`
/// is a no-op server-side and cache-side.
pub async fn update_nextcloud_calendar(
    calendar_id: String,
    display_name: Option<String>,
    color: Option<String>,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let (nc_id, path) = cache
        .get_calendar_server_path(&calendar_id)?
        .ok_or_else(|| UnkaiError::Other(format!("no cached calendar with id '{calendar_id}'")))?;
    let account = load_nextcloud_account(cache, &nc_id)?;

    // Local calendars have no server to PROPPATCH (#413) — the
    // cache metadata update below is the whole operation.
    if !account.is_local() {
        let app_password = credentials::get_nextcloud_password(&nc_id)?;
        caldav_update_calendar(
            &path,
            &account.username,
            &app_password,
            display_name.as_deref(),
            color.as_deref(),
            &account.trusted_certs,
        )
        .await?;
    }

    cache.update_calendar_metadata(&calendar_id, display_name.as_deref(), color.as_deref())?;
    Ok(())
}

/// Delete a calendar on the server + drop the cached row (events
/// cascade). The server's DELETE is destructive and irreversible on
/// most Nextcloud setups — callers (i.e. the UI) are expected to
/// confirm with the user before invoking this.
pub async fn delete_nextcloud_calendar(
    calendar_id: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let (nc_id, path) = cache
        .get_calendar_server_path(&calendar_id)?
        .ok_or_else(|| UnkaiError::Other(format!("no cached calendar with id '{calendar_id}'")))?;
    let account = load_nextcloud_account(cache, &nc_id)?;

    // Local calendars only exist in the cache (#413).
    if !account.is_local() {
        let app_password = credentials::get_nextcloud_password(&nc_id)?;
        caldav_delete_calendar(
            &path,
            &account.username,
            &app_password,
            &account.trusted_certs,
        )
        .await?;
    }
    cache.remove_calendar(&calendar_id)?;
    Ok(())
}

/// Layer 1: flip a calendar's sidebar visibility. Purely client-side —
/// no CalDAV traffic. `hidden = true` removes the calendar from the
/// sidebar entirely (controlled from NextcloudSettings).
pub fn set_nextcloud_calendar_hidden(
    calendar_id: String,
    hidden: bool,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    cache.set_calendar_hidden(&calendar_id, hidden)?;
    Ok(())
}

/// Layer 2: flip a calendar's event-grid visibility. Purely client-side.
/// `muted = true` keeps the calendar in the sidebar but stops its events
/// from painting on the agenda grid (controlled via the sidebar swatch).
pub fn set_nextcloud_calendar_muted(
    calendar_id: String,
    muted: bool,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    cache.set_calendar_muted(&calendar_id, muted)?;
    Ok(())
}

/// Events in `[range_start, range_end)` across the given calendars,
/// with recurring series already expanded into concrete occurrences.
///
/// `calendar_ids` is the full set the UI wants to display at once —
/// typically every calendar belonging to a Nextcloud account, so one
/// round-trip paints the whole sidebar.
///
/// The expansion pipeline:
/// 1. `cache.list_events_for_expansion` returns three buckets of rows
///    — in-window singletons, all recurring masters, all overrides.
///    Masters and overrides are fetched un-windowed because a series'
///    master may predate the window but still have instances inside
///    it, and an override may have been moved from outside the window
///    to inside it (or vice versa).
/// 2. Overrides are indexed by the `{calendar_id}::{uid}` prefix of
///    their composite id — the very same prefix that a master's id
///    has — so matching an override to its series is O(1).
/// 3. `unkai_caldav::expand_event` does the RFC 5545 work: RRULE
///    enumeration, EXDATE removal, RDATE insertion, override swap-in.
///
/// Pull events out of the local cache for `calendar_ids` over
/// `[range_start, range_end)`, recurrence-expanded.  Shared by
/// `get_cached_events` (the calendar grid) and
/// `get_attendee_availability` (the planner's local-cache scan
/// for external attendees).
///
/// Mirrors the expansion pipeline documented on `get_cached_events`:
/// singletons + recurring masters + overrides → expand each master
/// against its overrides → sorted chronological list.
pub fn expand_calendar_events_in_range(
    cache: &Cache,
    calendar_ids: &[String],
    range_start: chrono::DateTime<chrono::Utc>,
    range_end: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<CalendarEvent>, UnkaiError> {
    let input = cache
        .list_events_for_expansion(calendar_ids, range_start, range_end)
        .map_err(UnkaiError::from)?;

    let mut overrides_by_master: std::collections::HashMap<&str, Vec<&CalendarEvent>> =
        std::collections::HashMap::new();
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

pub fn get_cached_events(
    calendar_ids: Vec<String>,
    range_start: chrono::DateTime<chrono::Utc>,
    range_end: chrono::DateTime<chrono::Utc>,
    cache: &Cache,
) -> Result<Vec<CalendarEvent>, UnkaiError> {
    expand_calendar_events_in_range(cache, &calendar_ids, range_start, range_end)
}

/// What the Svelte editor sends for a create or update. Matches the
/// `CalendarEvent` shape the UI already knows but flattens to plain
/// strings / booleans the Tauri IPC layer can serialise without
/// extra adapters. Optional fields stay optional so the form can
/// submit a partial event without leaving phantom values behind.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEventInput {
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
    /// True for events the user picked "All day" on. The server stores
    /// these as `VALUE=DATE` ranges; we re-derive that from the start /
    /// end times being a midnight…23:59:59 window.
    #[serde(default)]
    pub all_day: bool,
    #[serde(default)]
    pub url: Option<String>,
    /// `OPAQUE` (busy) or `TRANSPARENT` (free). Matches the editor's
    /// "show as" picker. `None` means "leave whatever the server had".
    #[serde(default)]
    pub transparency: Option<String>,
    #[serde(default)]
    pub attendees: Vec<EventAttendee>,
    #[serde(default)]
    pub reminders: Vec<EventReminder>,
    /// `GEO` latitude / longitude (RFC 5545 §3.8.1.6).  Set by the
    /// EventEditor's location-autocomplete pick (#280); `None`
    /// when the user typed the location free-text without
    /// selecting a geocoded match.
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
}

/// Build a `CalendarEvent` skeleton from form input. Caller fills in
/// `id` (a fresh UID for create, the cached UID for update). Recurrence
/// fields stay empty here — the editor doesn't expose them yet, and
/// any existing recurrence is preserved from the cached event by the
/// update command before this struct is rebuilt.
pub fn input_to_calendar_event(uid: &str, input: &CalendarEventInput) -> CalendarEvent {
    // For all-day events the editor sends midnight UTC starts; snap
    // the end to 23:59:59 of the last covered day so `build_ics`
    // recognises the all-day shape. For timed events we trust the
    // editor's exact instants.
    let (start, end) = if input.all_day {
        use chrono::TimeZone;
        let start_day = input.start.date_naive();
        let end_day = input.end.date_naive();
        let s = chrono::Utc.from_utc_datetime(&start_day.and_hms_opt(0, 0, 0).unwrap());
        let e = chrono::Utc.from_utc_datetime(&end_day.and_hms_opt(23, 59, 59).unwrap());
        (s, e)
    } else {
        (input.start, input.end)
    };
    CalendarEvent {
        id: uid.to_string(),
        summary: input.summary.clone(),
        description: input.description.clone(),
        start,
        end,
        location: input.location.clone(),
        rrule: None,
        rdate: vec![],
        exdate: vec![],
        recurrence_id: None,
        url: input.url.clone(),
        transparency: input.transparency.clone(),
        attendees: input.attendees.clone(),
        reminders: input.reminders.clone(),
        latitude: input.latitude,
        longitude: input.longitude,
    }
}

/// Convert a `CalendarEvent` (post-write) into the row shape the cache
/// expects. Used by both `create_calendar_event` and
/// `update_calendar_event` so the local cache reflects the new state
/// without waiting for the next sync round.
pub fn calendar_event_to_row(
    event: &CalendarEvent,
    href: &str,
    etag: &str,
    ics_raw: &str,
) -> CalendarEventRow {
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

/// Resolve the `(email, display_name)` to write into `ORGANIZER`
/// for an outbound VEVENT.  This drives whether NC's iMIP plugin
/// can route the invite via the user's real Mail-app SMTP (NC 30+
/// Mail Provider): the address must match the user's primary
/// email exactly, otherwise NC falls back to the system mailer
/// with `From: invitations-noreply@…`.
///
/// Strategy:
/// 1. **When attendees are present**, fetch the user's profile
///    from `/ocs/v2.php/cloud/user`.  Its `email` field is what
///    NC's Mail Provider keys against — same source of truth NC
///    uses internally, so we can't get it wrong.
/// 2. **When the OCS lookup fails or returns no email**, fall
///    back to `organizer_local` (username if it parses as an
///    email, else `username@server-host`) so the PUT still
///    succeeds.  The fallback may not match a Mail-app account,
///    in which case NC's system mailer takes over — better than
///    failing the save.
/// 3. **When there are no attendees**, skip the network call
///    entirely and use the local fallback.  NC's scheduling plugin
///    won't fire without attendees, so `ORGANIZER` here is just
///    metadata for the calendar copy.
pub async fn resolve_organizer(
    account: &NextcloudAccount,
    app_password: &str,
    has_attendees: bool,
) -> (String, Option<String>) {
    if !has_attendees {
        return organizer_local(account);
    }
    match unkai_nextcloud::user::fetch_current_user(
        &account.server_url,
        &account.username,
        app_password,
        &account.trusted_certs,
    )
    .await
    {
        Ok(profile) => {
            if let Some(email) = profile.email {
                let name = profile
                    .display_name
                    .or_else(|| account.display_name.clone());
                return (email, name);
            }
            tracing::warn!(
                "Nextcloud user has no email set in Personal info — \
                 iMIP will fall back to system mailer"
            );
        }
        Err(e) => tracing::warn!("OCS user lookup failed, using fallback ORGANIZER: {e}"),
    }
    organizer_local(account)
}

/// Local-only fallback when we can't reach OCS.  Same shape we used
/// before: prefer `username` when it's already an email, else
/// synthesise `username@host`.  This is unrouteable on the public
/// internet but satisfies Sabre's "ATTENDEE without ORGANIZER is
/// 403" check so the PUT itself succeeds.
pub fn organizer_local(account: &NextcloudAccount) -> (String, Option<String>) {
    let email = if account.username.contains('@') {
        account.username.clone()
    } else {
        let host = account
            .server_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .split('/')
            .next()
            .unwrap_or("nextcloud.local");
        format!("{}@{}", account.username, host)
    };
    (email, account.display_name.clone())
}

/// Inspect a CalDAV-write error for the 403/404 permission signal
/// and, if present, mark the affected calendar as `read_only` in
/// the local cache.  Best-effort: failures here are logged and
/// dropped — the user already saw the upstream write fail; the
/// only loss from a missed flip is that they'd see the same
/// error again on the next attempt.
///
/// Emits `calendars-updated` so the EventEditor (already open
/// with the failed save) refreshes its `calendars` prop and the
/// `currentCalendarReadOnly` derived flips, hiding Save + Delete.
pub fn flag_calendar_read_only_on_forbidden(
    ui: &dyn UiNotifier,
    cache: &Cache,
    calendar_id: &str,
    err: &UnkaiError,
) {
    if !matches!(err, UnkaiError::CalDavWriteForbidden(_)) {
        return;
    }
    if let Err(e) = cache.set_calendar_read_only(calendar_id, true) {
        tracing::warn!(
            "failed to flip read_only=true on calendar '{calendar_id}' after CalDAV 403/404: {e}"
        );
        return;
    }
    tracing::info!(
        "calendar '{calendar_id}' marked read-only locally after CalDAV write was forbidden"
    );
    // Resolve the NC account id from the calendar id (`{nc}::{path}`)
    // so the frontend listener can scope its refresh — costs nothing
    // to include and lets a future multi-account UI avoid blanket
    // refetches.
    let nc_account_id = calendar_id.split_once("::").map(|(nc, _)| nc.to_string());
    ui.calendars_updated(&CalendarsUpdatedPayload {
        nextcloud_account_id: nc_account_id,
    });
}

/// The PUT uses `If-None-Match: *`, so a UID collision surfaces as
/// a structured error instead of a silent overwrite. On success, the
/// new event is upserted into the local cache so the UI can render it
/// without waiting for the next sync.
pub async fn create_calendar_event(
    calendar_id: String,
    input: CalendarEventInput,
    cache: &Cache,
    ui: &dyn UiNotifier,
) -> Result<CalendarEvent, UnkaiError> {
    let (nc_id, calendar_path) =
        cache
            .get_calendar_server_path(&calendar_id)?
            .ok_or_else(|| {
                UnkaiError::Other(format!(
                    "calendar '{calendar_id}' is not in the local cache — refresh and try again"
                ))
            })?;
    let account = load_nextcloud_account(cache, &nc_id)?;

    let uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let event = input_to_calendar_event(&uid, &input);
    // Local sources can't (and needn't) ask an OCS endpoint who the
    // organizer is (#413) — fall back to the account-derived line.
    let (organizer_email, organizer_name) = if account.is_local() {
        organizer_local(&account)
    } else {
        let app_password = credentials::get_nextcloud_password(&nc_id)?;
        resolve_organizer(&account, &app_password, !event.attendees.is_empty()).await
    };
    let ics = caldav_build_ics(&event, Some(&organizer_email), organizer_name.as_deref());

    // `calendar_path` from the cache is already an absolute URL —
    // `unkai-caldav::discovery` resolves it via `absolute_url` before
    // storing. Don't re-prefix the server origin or the PUT goes to
    // `https://hosthttps://host/...`.
    let outcome = dav_create_event_for(&account, &calendar_path, &uid, &ics)
        .await
        .inspect_err(|e| flag_calendar_read_only_on_forbidden(ui, cache, &calendar_id, e))?;

    let row = calendar_event_to_row(&event, &outcome.href, &outcome.etag, &ics);
    cache.upsert_single_event(&calendar_id, &row)?;

    // Re-derive the app-side id the same way `event_row_id` does so the
    // returned event matches what `get_cached_events` will surface.
    let mut out = event;
    out.id = format!("{calendar_id}::{uid}");
    Ok(out)
}

/// Update an existing VEVENT, keyed by its app-side id.
///
/// Preserves the cached UID and href; everything else comes from the
/// editor input. The PUT is gated on the cached etag so a concurrent
/// edit on another device surfaces as a structured error (412 → human-
/// readable string) instead of overwriting the other change silently.
pub async fn update_calendar_event(
    event_id: String,
    input: CalendarEventInput,
    cache: &Cache,
    ui: &dyn UiNotifier,
) -> Result<CalendarEvent, UnkaiError> {
    let handle = load_event_handle(cache, &event_id)?;
    let account = load_nextcloud_account(cache, &handle.nextcloud_account_id)?;

    let mut event = input_to_calendar_event(&handle.uid, &input);
    // Preserve recurrence info the editor doesn't surface — losing it
    // would silently demote a recurring series back to a singleton.
    event.recurrence_id = handle.recurrence_id;

    // Only a real Nextcloud has the OCS profile endpoint behind
    // `resolve_organizer` — local sources have no keychain entry at
    // all, and generic DAV servers would just eat a dead request
    // (#413).
    let (organizer_email, organizer_name) = if account.is_nextcloud() {
        let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;
        resolve_organizer(&account, &app_password, !event.attendees.is_empty()).await
    } else {
        organizer_local(&account)
    };
    let ics = caldav_build_ics(&event, Some(&organizer_email), organizer_name.as_deref());
    // Use the etag-aware retry helper so a concurrent edit on
    // another device (NC web, phone) doesn't surface to the
    // user as "refresh and try again" — it transparently syncs
    // and re-PUTs once.
    let outer_calendar_id = handle.calendar_id.clone();
    let (outcome, handle) = update_event_with_etag_retry(cache, &event_id, &ics)
        .await
        .inspect_err(|e| flag_calendar_read_only_on_forbidden(ui, cache, &outer_calendar_id, e))?;

    let row = calendar_event_to_row(&event, &outcome.href, &outcome.etag, &ics);
    cache.upsert_single_event(&handle.calendar_id, &row)?;

    let mut out = event;
    out.id = event_id;
    Ok(out)
}

/// Delete an event from the server and the local cache.  Server-side
/// iTIP CANCEL notices to attendees are emitted by Nextcloud's
/// `OCA\DAV\CalDAV\Schedule\IMipPlugin` on the DELETE — no
/// client-side mail involved.
pub async fn delete_calendar_event(
    event_id: String,
    cache: &Cache,
    ui: &dyn UiNotifier,
) -> Result<(), UnkaiError> {
    let handle = load_event_handle(cache, &event_id)?;
    let calendar_id = handle.calendar_id.clone();
    delete_event_with_etag_retry(cache, &event_id, &handle)
        .await
        .inspect_err(|e| flag_calendar_read_only_on_forbidden(ui, cache, &calendar_id, e))?;
    cache.delete_event_by_id(&event_id)?;
    Ok(())
}

// ── Scheduling-assistant availability (#137) ─────────────────
//
// `get_attendee_availability` powers the EventPlanner UI: given a
// list of attendee email addresses and a time window, return each
// person's busy slots so the UI can render a free/busy grid.
//
// Resolution order per attendee:
//
//   1. **Sharees lookup** — does this address belong to a local NC
//      user?  If yes, run a CalDAV `free-busy-query` REPORT against
//      their calendar home.  Returns busy periods only (no event
//      details), so the privacy story is identical to the standard
//      Nextcloud / Outlook free-busy lookup users already expect.
//   2. **Free-busy succeeded** → emit them with `source =
//      "nc-freebusy"`.  This is the authoritative signal.
//   3. **Free-busy failed** (server refused, calendar not shared,
//      network blip) → fall through to the local-cache scan.
//   4. **Not an NC user, or NC lookup failed** → scan our own
//      calendars for events where this address is listed as an
//      attendee.  Surfaces the meetings *we* know about that the
//      person was invited to.  Issued via `source = "local-cache"`.
//   5. **Anything else** → empty list with `source = "unknown"`.
//      The UI renders the row as "no signal — assume free".
//
// The local-cache scan piggybacks on the existing recurrence-
// expanded `expand_calendar_events_in_range` so a series the
// attendee was invited to surfaces every occurrence in the window.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttendeeBusyPeriod {
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
    /// One of "busy", "tentative", "unavailable", "free".  The
    /// planner UI maps these to its own colour palette.
    pub kind: String,
    /// Source event's summary, when the period came from our
    /// local-cache scan (the user's own calendars where the
    /// attendee is listed).  CalDAV free-busy responses
    /// deliberately don't carry titles — privacy — so this
    /// stays `None` for `nc-freebusy` periods.  Surfacing it
    /// in the planner is fine because the user already owns
    /// the event whose title we're showing.
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttendeeAvailability {
    pub email: String,
    pub display_name: Option<String>,
    /// "nc-freebusy" | "local-cache" | "unknown" — see resolution
    /// order in the module-level comment above.
    pub source: String,
    pub busy_periods: Vec<AttendeeBusyPeriod>,
}

pub fn busy_kind_to_string(k: CaldavBusyKind) -> String {
    match k {
        CaldavBusyKind::Busy => "busy",
        CaldavBusyKind::Tentative => "tentative",
        CaldavBusyKind::Unavailable => "unavailable",
        CaldavBusyKind::Free => "free",
    }
    .to_string()
}

pub async fn get_attendee_availability(
    nc_id: String,
    attendee_emails: Vec<String>,
    range_start: chrono::DateTime<chrono::Utc>,
    range_end: chrono::DateTime<chrono::Utc>,
    cache: &Cache,
) -> Result<Vec<AttendeeAvailability>, UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    // Sharees lookup + free-busy are Nextcloud OCS/scheduling
    // features. Non-Nextcloud sources (#413) skip every network
    // step below (the sharees gate keeps `nc_match` at None) and
    // degrade to the local-cache scan — the empty password is
    // never sent anywhere.
    let app_password = if account.is_nextcloud() {
        credentials::get_nextcloud_password(&nc_id)?
    } else {
        String::new()
    };

    // Pre-load the local-cache events once so the per-attendee
    // scan loop doesn't repeat the SQL + expansion work.
    let calendar_ids: Vec<String> = cache
        .list_calendars(&nc_id)?
        .into_iter()
        .map(|c| c.id)
        .collect();
    let local_events =
        expand_calendar_events_in_range(cache, &calendar_ids, range_start, range_end)?;

    let mut out: Vec<AttendeeAvailability> = Vec::with_capacity(attendee_emails.len());

    for email in attendee_emails {
        let lower = email.trim().to_ascii_lowercase();
        if lower.is_empty() {
            continue;
        }

        // Step 1: sharees lookup.  Soft-fail (None) on errors so a
        // single bad lookup doesn't blank out the planner.  Skipped
        // entirely for non-Nextcloud sources (#413) — no OCS there.
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
                    tracing::info!("sharees lookup for '{email}' failed: {e}");
                    None
                }
            }
        };

        // Always pre-compute the local-cache hits — events from
        // the user's own calendars (which include shared/subscribed
        // calendars in NC) where this person is listed.  Used both
        // as the fallback when free-busy fails AND as a title
        // source to enrich free-busy periods that come back without
        // names attached.
        let local_for_attendee: Vec<(
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
            Option<String>,
        )> = local_events
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
                    if ev.summary.trim().is_empty() {
                        None
                    } else {
                        Some(ev.summary.clone())
                    },
                )
            })
            .collect();

        // Step 2: NC user → free-busy-query.
        if let Some(m) = nc_match.as_ref() {
            let principal_url = caldav_nc_principal_home(&account.server_url, &m.user_id);
            match caldav_query_free_busy(
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
                    out.push(AttendeeAvailability {
                        email,
                        display_name: Some(m.display_name.clone()),
                        source: "nc-freebusy".into(),
                        busy_periods: periods
                            .into_iter()
                            .map(|p| AttendeeBusyPeriod {
                                start: p.start,
                                end: p.end,
                                kind: busy_kind_to_string(p.kind),
                                // Free-busy responses themselves
                                // don't carry titles by design,
                                // but if we *also* have the same
                                // event in our local cache (the
                                // attendee invited us, or their
                                // calendar is shared with us),
                                // surface its title — that's not
                                // a privacy regression because we
                                // already own the data on our
                                // side.  Match on start time;
                                // server-side regeneration of
                                // free-busy uses the source
                                // event's DTSTART verbatim.
                                summary: local_for_attendee
                                    .iter()
                                    .find(|(s, _, _)| *s == p.start)
                                    .and_then(|(_, _, sum)| sum.clone()),
                            })
                            .collect(),
                    });
                    continue;
                }
                Err(e) => {
                    // Common case: the calendar isn't shared with
                    // us, so the REPORT 403/404s.  Drop down to the
                    // local-cache scan.
                    tracing::info!(
                        "free-busy-query unavailable for {} ({email}): {e}",
                        m.user_id
                    );
                }
            }
        }

        // Step 3: local-cache fallback — events in the user's own
        // calendars where this person is listed as an attendee.
        let busy: Vec<AttendeeBusyPeriod> = local_for_attendee
            .iter()
            .map(|(start, end, summary)| AttendeeBusyPeriod {
                start: *start,
                end: *end,
                kind: "busy".into(),
                summary: summary.clone(),
            })
            .collect();

        let display_name = nc_match.as_ref().map(|m| m.display_name.clone());
        let source = if !busy.is_empty() {
            "local-cache"
        } else if nc_match.is_some() {
            // We knew it was an NC user but free-busy failed and
            // we have no local events for them — leave the row
            // empty with `unknown` so the UI distinguishes "no
            // signal" from "confirmed free".
            "unknown"
        } else {
            "local-cache"
        }
        .to_string();

        out.push(AttendeeAvailability {
            email,
            display_name,
            source,
            busy_periods: busy,
        });
    }

    Ok(out)
}

// ── Location autocomplete + map preview (#280) ───────────────
//
// The EventEditor's Location field offers two affordances:
//
//   1. **Autocomplete** — keystrokes (debounced) call
//      `geocode_search`, which dedupes against the local
//      `geocode_cache` table before hitting Nominatim.  Picking
//      a suggestion stamps the canonical `display_name` plus
//      `(lat, lon)` onto the in-flight event, which then
//      round-trips through `LOCATION` + `GEO` in the iCalendar
//      body.
//
//   2. **Inline map preview** — once the event has a `(lat,
//      lon)`, the UI mounts a small MapLibre canvas pointing at
//      it.  All tile traffic goes to public OSM-backed tile
//      services with attribution (see the frontend component).
//
// `detect_nc_maps` is informational: it tells the UI whether
// the user's connected NC has the Maps app enabled so the UI
// can surface "Using your Nextcloud Maps" in the autocomplete
// header.  The actual geocoding still goes to Nominatim either
// way — NC Maps doesn't expose a server-side proxy at present.

pub async fn geocode_search(
    query: String,
    lang: Option<String>,
    cache: &Cache,
    settings: &SharedSettings,
) -> Result<Vec<crate::geocode::GeocodeResult>, UnkaiError> {
    // Privacy gate (#280).  Off by default; the user must opt in
    // via General Settings before any keystroke leaves the
    // device.  We refuse here as well as in the UI so a
    // mis-wired component can't accidentally exfiltrate a query
    // before the toggle's state propagates.
    //
    // We snapshot both the toggle and the configurable
    // `nominatim_base_url` under the same read so a settings
    // change between the two reads can't have us call out to
    // a stale endpoint after the toggle was just flipped on.
    let (enabled, base_url) = {
        let s = settings.read().await;
        (s.location_geocoding_enabled, s.nominatim_base_url.clone())
    };
    if !enabled {
        return Ok(Vec::new());
    }

    let lang = lang.unwrap_or_default();
    // Cache hit short-circuits the network round-trip.  The
    // cache itself canonicalises the query (whitespace,
    // case-folding) so a tiny stylistic typo doesn't burn an
    // upstream call.
    if let Some(json) = cache
        .get_geocode_cache(&query, &lang)
        .map_err(UnkaiError::from)?
    {
        if let Ok(hits) = serde_json::from_str::<Vec<crate::geocode::GeocodeResult>>(&json) {
            return Ok(hits);
        }
        // Cache row exists but is corrupt — fall through to a
        // fresh fetch and let the new payload overwrite it.
        tracing::warn!("geocode_cache: corrupt row for {query:?}, refetching");
    }

    let hits = crate::geocode::nominatim_search(&query, &lang, &base_url).await?;
    let serialised = serde_json::to_string(&hits)
        .map_err(|e| UnkaiError::Other(format!("geocode result serialise: {e}")))?;
    if let Err(e) = cache.put_geocode_cache(&query, &lang, &serialised) {
        // Cache write failure is non-fatal — the user still
        // gets the live result.
        tracing::warn!("geocode_cache write failed: {e}");
    }
    Ok(hits)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NextcloudMapsCapability {
    /// True when the connected NC has the Maps app enabled.
    pub available: bool,
}

pub async fn detect_nc_maps(
    nc_id: String,
    cache: &Cache,
) -> Result<NextcloudMapsCapability, UnkaiError> {
    let account = load_nextcloud_account(cache, &nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    // The capabilities OCS endpoint returns an enabled-apps map
    // we can scan for `maps` without needing to actually call
    // any Maps-app endpoints.  Soft-fails to "not available"
    // on any network blip — the UI just shows the generic
    // OSM-attribution copy in that case.
    let server = account.server_url.trim_end_matches('/');
    let url = format!("{server}/ocs/v2.php/cloud/capabilities?format=json");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| UnkaiError::Network(format!("capabilities client: {e}")))?;
    let resp = client
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(&account.username, Some(&app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("capabilities request: {e}")))?;
    if !resp.status().is_success() {
        return Ok(NextcloudMapsCapability { available: false });
    }
    // The capabilities body is deeply nested; we just look for
    // any case-insensitive hint of "maps" inside the
    // capabilities key.  More precise parsing would tie us to
    // an NC version's exact JSON shape — this is informational
    // anyway, and a false negative just means the UI doesn't
    // show the "via NC Maps" hint.
    let body = resp.text().await.unwrap_or_default();
    let available = body.to_ascii_lowercase().contains("\"maps\"");
    Ok(NextcloudMapsCapability { available })
}

/// Remove a locally-cached event whose iCalendar `UID` matches
/// `uid`.  Surfaced from the inbound CANCEL card in MailView:
/// when an external organiser sends a `METHOD:CANCEL` mail, the
/// user clicks "Remove from my calendar" and we DELETE the
/// CalDAV resource so the cancelled meeting disappears from the
/// grid (and from any other CalDAV client, including their
/// phone).  Idempotent: returns `Ok(())` when no row matches —
/// the user may have already removed the event manually, or the
/// invite never made it into their calendar in the first place.
///
/// Note that we don't fight Sabre's iTIP machinery here.  An
/// attendee-side DELETE of an event whose ORGANIZER is external
/// would normally generate a `METHOD:REPLY;PARTSTAT=DECLINED`
/// from NC's IMipPlugin; that's not what we want when responding
/// to a CANCEL (the organiser already cancelled — a "decline" is
/// noise).  In practice Sabre suppresses REPLY emission when the
/// stored event already carries `STATUS:CANCELLED` or the user's
/// PARTSTAT is unchanged from the previous version, which covers
/// the common case.  Worth flagging explicitly if it turns out
/// to send spurious mail in the wild.
/// True when an event with the given iCalendar UID exists in
/// any of the user's locally-cached calendars.  Used by the
/// CANCEL card to decide whether to expose "Remove from my
/// calendar" — only makes sense when there's actually a local
/// copy to remove.  A miss here is the common case for invites
/// the user never accepted (CANCEL arrives but the event was
/// never imported into a calendar): the card should fall back
/// to a passive "not in your calendar" line instead of the
/// remove button.
pub fn is_event_in_calendar(uid: String, cache: &Cache) -> Result<bool, UnkaiError> {
    Ok(cache.find_event_id_by_uid(&uid)?.is_some())
}

/// Record that an iCalendar UID has been cancelled by its
/// organiser.  Called by MailView when it surfaces a
/// `METHOD:CANCEL` mail, so the original REQUEST mail's RSVP
/// card can flip to the cancelled flavour on its next open.
pub fn record_cancelled_invite(uid: String, cache: &Cache) -> Result<(), UnkaiError> {
    cache.mark_invite_cancelled(&uid).map_err(UnkaiError::from)
}

/// True when MailView has previously observed a `METHOD:CANCEL`
/// mail for this iCalendar UID.  Used by the RSVP card to
/// flip the original REQUEST mail's flavour to the cancelled
/// banner so the user doesn't unwittingly answer a meeting
/// that's been cancelled.
pub fn is_invite_cancelled(uid: String, cache: &Cache) -> Result<bool, UnkaiError> {
    cache.is_invite_cancelled(&uid).map_err(UnkaiError::from)
}

pub async fn dismiss_cancelled_event(uid: String, cache: &Cache) -> Result<(), UnkaiError> {
    let Some(event_id) = cache.find_event_id_by_uid(&uid)? else {
        tracing::info!(
            "dismiss_cancelled_event: no cached event with UID {uid}, treating as no-op"
        );
        return Ok(());
    };
    let handle = load_event_handle(cache, &event_id)?;
    let account = load_nextcloud_account(cache, &handle.nextcloud_account_id)?;
    // A cancelled invite living on a local calendar only exists in
    // the cache — the delete below is the whole dismissal (#413).
    if !account.is_local() {
        let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;
        // Use the silent variant — without `Schedule-Reply: F`,
        // Sabre's attendee-side DELETE handler emits a spurious
        // `METHOD:REPLY;PARTSTAT=DECLINED` to the organiser.  The
        // organiser already sent CANCEL; mailing them a decline
        // back is just noise (and confusing).
        unkai_caldav::delete_event_silent(
            &handle.href,
            &account.username,
            &app_password,
            &handle.etag,
            &account.trusted_certs,
        )
        .await?;
    }
    cache.delete_event_by_id(&event_id)?;
    Ok(())
}

// ── iTIP / iMIP (#58) ─────────────────────────────────────────────
//
// Outbound: when Compose's "Add Event" flow saves an event, we hand
// the recipient mail clients a `text/calendar; method=REQUEST`
// attachment so any RFC-compliant client can save the invite
// natively.
//
// Inbound: when a received message carries a `text/calendar` part,
// we parse the iCalendar and surface an "invite card" with
// Accept / Decline / Tentative buttons.  Each click silently
// emits a `text/calendar; method=REPLY` email back to the
// organiser — that's the iMIP RSVP loop (RFC 6047).

/// Lightweight iCalendar summary the JS layer renders for an
/// inbound invite (Accept / Decline / Tentative card).  Picks
/// the smallest set of fields the card needs; the full ICS bytes
/// stay on the Rust side and ride through `send_event_rsvp` so
/// the REPLY can carry the same UID and DTSTAMP without the
/// frontend having to round-trip the full event.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteSummary {
    /// Calendar-level `METHOD:` value, upper-cased.  iTIP defines
    /// REQUEST (organiser → attendee), REPLY (attendee →
    /// organiser), CANCEL, PUBLISH, REFRESH, COUNTER, DECLINECOUNTER.
    /// `MailView` only shows the RSVP card for REQUEST; the others
    /// (especially REPLY) are typically attendee responses to OUR
    /// invites and don't need a "you can RSVP" card on the
    /// organiser's side.  `None` means no METHOD line was present
    /// (treat as "not an iTIP message" and suppress the card).
    pub method: Option<String>,
    /// VEVENT UID — the join key between REQUEST + REPLY.
    pub uid: String,
    /// SUMMARY (title) of the event.
    pub summary: String,
    /// DTSTART, normalised to UTC (RFC 3339).
    pub start: chrono::DateTime<chrono::Utc>,
    /// DTEND, normalised to UTC.
    pub end: chrono::DateTime<chrono::Utc>,
    /// Optional venue / room.
    pub location: Option<String>,
    /// Optional URL — Talk join links, video conferencing, etc.
    pub url: Option<String>,
    /// ORGANIZER's email (mailto: URI stripped).  Required by RFC
    /// 5546 whenever any ATTENDEE is present, so we expect it on
    /// real-world invites — but a missing one isn't fatal, the
    /// RSVP just falls back to the message's From: address.
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    /// All ATTENDEEs from the VEVENT.  The card highlights the
    /// row matching the current user's address so they can see
    /// their own NEEDS-ACTION status before clicking.
    pub attendees: Vec<unkai_core::models::EventAttendee>,
    /// The full ICS body, used to preserve UID + DTSTAMP +
    /// SEQUENCE on the REPLY without re-fetching.
    pub raw_ics: String,
}

/// Parse a raw `text/calendar` byte slice into the slim
/// `InviteSummary` the inbound RSVP card consumes.  Looks at the
/// FIRST VEVENT in the file — recurring series and overrides are
/// out of scope for the invite card MVP (the user can still
/// manage them in the Calendar view after accepting).
///
/// `parse_ics` doesn't surface ORGANIZER as a typed field today,
/// so the JS caller is expected to fall back to the message's
/// `From:` header for the recipient of the RSVP REPLY — which is
/// what RFC 5546 says the organiser address tracks anyway.
pub fn parse_event_invite(bytes: Vec<u8>) -> Result<InviteSummary, UnkaiError> {
    let body = String::from_utf8(bytes)
        .map_err(|e| UnkaiError::Protocol(format!("invite is not UTF-8: {e}")))?;
    let events = unkai_caldav::ical::parse_ics(&body)
        .map_err(|e| UnkaiError::Protocol(format!("could not parse calendar invite: {e}")))?;
    let event = events
        .into_iter()
        .next()
        .ok_or_else(|| UnkaiError::Protocol("invite contains no VEVENT".into()))?;

    let method = extract_calendar_method(&body);

    Ok(InviteSummary {
        method,
        uid: event.id.clone(),
        summary: event.summary.clone(),
        start: event.start,
        end: event.end,
        location: event.location.clone(),
        url: event.url.clone(),
        organizer_email: None,
        organizer_name: None,
        attendees: event.attendees.clone(),
        raw_ics: body,
    })
}

/// Pull the calendar-level `METHOD:` value out of a raw ICS body
/// without round-tripping through a full parser.  iTIP defines
/// the line as a single token after the colon (REQUEST / REPLY /
/// CANCEL / etc.); we just normalise to upper case so JS-side
/// equality checks don't have to be case-insensitive.
pub fn extract_calendar_method(ics: &str) -> Option<String> {
    for line in ics.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("METHOD:") {
            let m = rest.trim();
            if !m.is_empty() {
                return Some(m.to_uppercase());
            }
        }
    }
    None
}

/// Generate a `METHOD:REPLY` iCalendar body for the user's RSVP
/// response.  Re-renders the original event with PARTSTAT updated
/// for the current user's ATTENDEE row only — every other
/// ATTENDEE keeps whatever the inbound message had so the
/// organiser's mail client doesn't see spurious "everyone
/// changed" diffs.
///
/// Respond to an inbound invite by writing the user's PARTSTAT to
/// CalDAV.  Nextcloud's CalDAV-Schedule plugin (with NC 30+ Mail
/// Provider) generates and SMTPs the iMIP REPLY automatically —
/// the client never touches SMTP for RSVPs.
///
/// Behaviour by partstat:
/// - **ACCEPTED**: PUT into `calendar_id` with PARTSTAT=ACCEPTED,
///   TRANSP=OPAQUE.  The event lands on the user's calendar (and
///   syncs to their phone), and NC mails the organiser.
/// - **TENTATIVE**: PUT with PARTSTAT=TENTATIVE, TRANSP=TRANSPARENT
///   so the calendar can render it visually distinct (striped
///   pattern in CalendarView).
/// - **DECLINED**: PUT with PARTSTAT=DECLINED, then DELETE the
///   resource.  The PUT triggers NC's REPLY (organiser notified);
///   the DELETE removes the entry from the user's calendar so
///   declined meetings don't clutter the grid.
///
/// Resolving the responding attendee's address goes through
/// **every identity Unkai knows about**, not just one: the NC
/// user-profile email (Sabre's principal CUA), every configured
/// mail-account address, plus an optional `attendee_email`
/// hint from the card (the address the inbound mail was
/// actually sent to).  We intersect that combined set with the
/// inbound ATTENDEE list and use whichever address is *already
/// in the invite* — that's the row Sabre's iTIP broker will
/// match on the user's principal-CUA when generating the
/// REPLY iMIP.
///
/// Why so many sources?  The chain is fragile: NC profile
/// email → Sabre principal CUA → ATTENDEE-row match →
/// IMipPlugin Mail Provider lookup against Mail-app accounts.
/// All four addresses must equal each other for REPLY mail to
/// actually leave NC.  Pinning to a single source means a
/// single misconfiguration (empty NC profile email, mismatched
/// Mail-app primary, etc.) silently breaks REPLY delivery —
/// exactly what was happening before.
// `attendee_hint`: optional hint from the card — the address
// the inbound mail was actually sent to, resolved by the
// frontend from the invite's ATTENDEE list intersected with
// the user's configured mail-account addresses.  Used as the
// highest-priority candidate when picking the row to mutate +
// identify with on Sabre's principal CUA.  May be `None` if
// the card couldn't resolve one.
pub async fn respond_to_invite(
    calendar_id: String,
    raw_ics: String,
    partstat: String,
    attendee_hint: Option<String>,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    // Resolve the chosen calendar's location on the server.
    let (nc_id, calendar_path) =
        cache
            .get_calendar_server_path(&calendar_id)?
            .ok_or_else(|| {
                UnkaiError::Other(format!(
                    "calendar '{calendar_id}' is not in the local cache — refresh and try again"
                ))
            })?;
    let account = load_nextcloud_account(cache, &nc_id)?;
    // Local calendars record the RSVP in the cache but there is no
    // server-side iTIP broker, so no REPLY email reaches the
    // organiser (#413). The empty password is never sent anywhere —
    // every network call below is skipped or no-ops for local.
    let app_password = if account.is_local() {
        String::new()
    } else {
        credentials::get_nextcloud_password(&nc_id)?
    };

    // Build the candidate-identity list, in priority order:
    //   1. The card's hint (transport-derived, most likely
    //      verbatim in the invite).
    //   2. NC profile email — Sabre's principal CUA, the
    //      authoritative identity for the iTIP broker.
    //   3. Every configured mail-account address (covers the
    //      "I added a Unkai mail account whose email differs
    //      from my NC profile" case).
    //   4. The synth `username@server-host` as a last resort.
    // We then take the FIRST candidate that actually appears
    // in the inbound ATTENDEE list — Sabre will match the
    // same row when scanning the body for the principal's CUA.
    // If no candidate matches, we fall back to candidate #2
    // (NC profile email — the address Sabre's broker is most
    // likely to identify as ours) and add a fresh row, so the
    // server-side iTIP can still pair us against the principal.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(hint) = attendee_hint.as_deref() {
        let h = hint.trim();
        if !h.is_empty() {
            candidates.push(h.to_string());
        }
    }
    let nc_profile_email = if account.is_nextcloud() {
        match unkai_nextcloud::user::fetch_current_user(
            &account.server_url,
            &account.username,
            &app_password,
            &account.trusted_certs,
        )
        .await
        {
            Ok(p) => p.email,
            Err(e) => {
                tracing::warn!("RSVP: NC user-profile lookup failed ({e})");
                None
            }
        }
    } else {
        // Generic DAV / local sources have no OCS profile endpoint
        // (#413); the mail-account addresses below cover identity.
        None
    };
    if let Some(e) = nc_profile_email.as_deref() {
        candidates.push(e.to_string());
    }
    if let Ok(mail_accounts) = account_store::load_accounts(cache) {
        for a in mail_accounts {
            candidates.push(a.email);
        }
    }
    candidates.push(organizer_local(&account).0);
    // Lower-cased, deduplicated, preserving priority order.
    let mut seen = std::collections::HashSet::new();
    let candidates: Vec<String> = candidates
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .filter(|s| seen.insert(s.to_ascii_lowercase()))
        .collect();
    tracing::debug!("RSVP candidate identities: {candidates:?}");

    // Pick the first candidate already present in the inbound
    // ATTENDEE list.  If none match, default to the NC profile
    // email (so Sabre's broker matches the new row we'll add
    // against its principal CUA) — and last-ditch the first
    // non-empty candidate so we always have something.
    let attendee_email = {
        let inbound_attendees: Vec<String> = unkai_caldav::ical::parse_ics(&raw_ics)
            .ok()
            .and_then(|v| v.into_iter().next())
            .map(|e| e.attendees.into_iter().map(|a| a.email).collect())
            .unwrap_or_default();
        let inbound_set: std::collections::HashSet<String> = inbound_attendees
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        candidates
            .iter()
            .find(|c| inbound_set.contains(&c.to_ascii_lowercase()))
            .cloned()
            .or(nc_profile_email)
            .or_else(|| candidates.into_iter().next())
            .unwrap_or_else(|| organizer_local(&account).0)
    };
    tracing::info!("RSVP: using attendee identity {attendee_email}");

    // Parse the inbound ICS, flip the matching attendee's PARTSTAT,
    // and (for TENTATIVE) override TRANSP so the calendar renders
    // it differently.
    let events = unkai_caldav::ical::parse_ics(&raw_ics)
        .map_err(|e| UnkaiError::Protocol(format!("could not parse invite: {e}")))?;
    let mut event = events
        .into_iter()
        .next()
        .ok_or_else(|| UnkaiError::Protocol("invite has no VEVENT".into()))?;

    // Flip the matching ATTENDEE's PARTSTAT.  When no row
    // matches — common for aliases, forwarded invites, or any
    // case where the user's mail-account address differs from
    // what the organiser typed into ATTENDEE — we ADD a fresh
    // row with the user's address instead of failing.  Sabre's
    // iTIP broker keys "is this PUT an RSVP from this user?"
    // off the principal-email match against the ATTENDEE list,
    // and an inserted row satisfies that check exactly the same
    // as a mutated one.  A REPLY then goes out from NC's iMIP
    // plugin with PARTSTAT carrying the user's chosen response.
    let mut matched = false;
    for att in event.attendees.iter_mut() {
        if att.email.eq_ignore_ascii_case(attendee_email.trim()) {
            att.status = Some(partstat.clone());
            // Force iMIP dispatch on the responding row — see
            // EventAttendee::force_send_reply.  Without this,
            // Sabre may process the PARTSTAT change locally
            // but skip the outbound iMIP to the organiser if
            // its "should this notify?" heuristics decline.
            att.force_send_reply = true;
            matched = true;
        }
    }
    if !matched {
        tracing::info!(
            "RSVP for {attendee_email}: address not in original ATTENDEE list, \
             adding a new row with PARTSTAT={partstat}"
        );
        event.attendees.push(EventAttendee {
            email: attendee_email.trim().to_string(),
            common_name: None,
            status: Some(partstat.clone()),
            role: Some("REQ-PARTICIPANT".into()),
            force_send_reply: true,
        });
    }
    if partstat == "TENTATIVE" {
        event.transparency = Some("TRANSPARENT".into());
    } else {
        // ACCEPTED + DECLINED => OPAQUE so the slot blocks (or
        // would block, before the DECLINE-side DELETE wipes it).
        event.transparency = Some("OPAQUE".into());
    }

    // PUT strategy — Sabre's CalDAV-Schedule plugin only fires a
    // REPLY iMIP when it sees a PARTSTAT diff against the
    // previously-stored copy.  A fresh PUT with `If-None-Match: *`
    // creates the resource for the first time and Sabre treats it
    // as the *organiser* writing into their own calendar — no
    // REPLY emerges.  To force the broker to see a real change,
    // first-time PUTs go in two steps:
    //   1. CREATE with the user's row at PARTSTAT=NEEDS-ACTION
    //      (the same state the inbound REQUEST has).  No iTIP
    //      runs here — there's no diff to compare.
    //   2. UPDATE the same href with the user's chosen PARTSTAT.
    //      Sabre sees NEEDS-ACTION → ACCEPTED/TENTATIVE/DECLINED,
    //      generates a METHOD:REPLY iMIP, and IMipPlugin SMTPs it
    //      to ORGANIZER through the system mailer.
    // For events already in the user's cache (re-RSVP / changing
    // your mind), one update_event keyed on the cached etag is
    // enough — Sabre still sees the prior PARTSTAT and emits the
    // REPLY iMIP.

    // The local cache can fall out of sync with the server in
    // ways that matter here: a previous DECLINED RSVP runs PUT
    // followed by DELETE, and Sabre may "soft-delete" by
    // converting the DELETE into a PARTSTAT=DECLINED on the
    // existing resource (so the organiser still sees who
    // declined).  We dropped the local row, but the server still
    // has the resource — so when the user changes their mind,
    // `find_event_id_by_uid` returns None and we'd try to CREATE
    // a fresh resource with the same UID, which the server
    // rejects with 412 ("already exists").  Refresh the cache
    // via a single-calendar CalDAV sync first, so a soft-delete
    // bounces back into the cache and we route through the
    // update path.
    let mut existing_id = cache.find_event_id_by_uid(&event.id)?;
    if existing_id.is_none() {
        if let Err(e) = refresh_calendar_cache(cache, &nc_id, &calendar_path).await {
            tracing::warn!("RSVP: pre-PUT cache refresh failed (continuing): {e}");
        }
        existing_id = cache.find_event_id_by_uid(&event.id)?;
    }
    // Track the body we actually PUT — used to mirror into the
    // cache afterwards, so the next surgical edit operates on
    // the body that's really on the server (not a regenerated
    // approximation).
    let body_put: String;
    let put_outcome = match existing_id {
        Some(existing_id) => {
            // Surgical-edit path.  Sabre's iTIP broker only
            // dispatches REPLY iMIP when the diff between the
            // stored body and the new PUT is "clean" — just the
            // user's PARTSTAT.  Regenerating the body via
            // `build_ics` drops X-* properties / re-orders /
            // loses params and Sabre then accepts the PARTSTAT
            // change but suppresses the iTIP REPLY (the same
            // restriction NC's web UI works around by editing
            // only the one line).  We do the same here: pull
            // the cached body, surgically replace just the user's
            // ATTENDEE PARTSTAT (and add SCHEDULE-FORCE-SEND=
            // REPLY), preserve everything else byte-for-byte.
            let handle = load_event_handle(cache, &existing_id)?;
            let surgical = unkai_caldav::ical::surgical_set_partstat(
                &handle.ics_raw,
                &attendee_email,
                &partstat,
                true,
            );
            let (out, _) = update_event_with_etag_retry(cache, &existing_id, &surgical).await?;
            body_put = surgical;
            out
        }
        None => {
            // Step 1 with surgical edit on the inbound ICS so
            // the body Sabre stores as the "before" state is a
            // minimal mutation of the original — Sabre's iTIP
            // restrictions accept it cleanly.
            let step1_body = unkai_caldav::ical::surgical_set_partstat(
                &raw_ics,
                &attendee_email,
                "NEEDS-ACTION",
                false,
            );
            let first =
                dav_create_event_for(&account, &calendar_path, &event.id, &step1_body).await?;

            // Step 2 — update keyed on the etag we just got, with
            // the user's chosen PARTSTAT + SCHEDULE-FORCE-SEND.
            // Sabre sees a clean PARTSTAT-only diff against
            // step 1's stored body and dispatches the REPLY iMIP.
            let step2_body = unkai_caldav::ical::surgical_set_partstat(
                &raw_ics,
                &attendee_email,
                &partstat,
                true,
            );
            let out = dav_update_event_for(&account, &first.href, &first.etag, &step2_body).await?;
            body_put = step2_body;
            out
        }
    };

    // Mirror the new state into the local cache so CalendarView
    // shows the accepted/tentative event without waiting for the
    // next sync — and so the *next* surgical edit operates on
    // the body that's actually on the server.
    let row = calendar_event_to_row(&event, &put_outcome.href, &put_outcome.etag, &body_put);
    cache.upsert_single_event(&calendar_id, &row)?;

    // DECLINED used to also DELETE the resource here ("no
    // clutter").  That removed user-declined events from the
    // calendar entirely, which made the badge afterwards look
    // like a cancellation (the event wasn't on any calendar but
    // we had a persisted RSVP for it).  Apple Calendar's
    // approach is right: keep the declined event around with
    // PARTSTAT=DECLINED so it stays visible (faded /
    // struck-through in the grid).  CalendarView can render the
    // declined visual state separately; this command just stops
    // deleting the row.

    // Persist the chosen PARTSTAT keyed by UID so the inbox card
    // re-renders the right state on reopen.  This mirrors what's
    // now on the server but avoids a CalDAV round-trip just for
    // UI feedback.
    if let Err(e) = cache.upsert_rsvp_response(&event.id, &partstat) {
        tracing::warn!("failed to persist RSVP response for {}: {e}", event.id);
    }
    Ok(())
}

/// Look up the user's last RSVP answer (ACCEPTED / DECLINED /
/// TENTATIVE) for an iCalendar UID. The invite card calls this on
/// mount so a previously answered invite re-renders in its
/// post-reply state instead of showing fresh Accept/Decline buttons.
pub async fn get_rsvp_response(uid: String, cache: &Cache) -> Result<Option<String>, UnkaiError> {
    cache.get_rsvp_response(&uid).map_err(UnkaiError::from)
}

/// Read the responding-user's PARTSTAT off the cached calendar
/// event with `uid`, if any.  Source of truth for the inbox
/// RSVP card so it reflects PARTSTAT changes made via NC web
/// UI / the user's phone / any other CalDAV client — not just
/// the changes Unkai made itself (which is what the local
/// `rsvp_responses` table tracks).
///
/// Runs a **differential CalDAV sync** of the calendar that
/// contains the event before reading, so the card always
/// reflects the latest server state without requiring the user
/// to wait for the background-sync interval.  CalDAV's
/// sync-collection report is incremental (only the deltas since
/// the last sync token), so the round-trip is cheap even on
/// large calendars.
///
/// Identity matching uses the same candidate list
/// `respond_to_invite` builds: the optional `attendee_hint`
/// from the card, the NC profile email, every configured mail
/// account.  Returns `None` when no row matches (or the event
/// isn't in the cache).
pub async fn get_event_partstat_for_user(
    uid: String,
    attendee_hint: Option<String>,
    cache: &Cache,
) -> Result<Option<String>, UnkaiError> {
    let Some(event_id) = cache.find_event_id_by_uid(&uid)? else {
        return Ok(None);
    };
    let handle = cache
        .get_event_server_handle(&event_id)?
        .ok_or_else(|| UnkaiError::Other("stale calendar cache entry".into()))?;

    // Differential CalDAV sync of the parent calendar — picks
    // up PARTSTAT changes made via NC web UI / phone / any other
    // CalDAV client without waiting for the background-sync
    // interval.  Best-effort: a sync failure leaves the cache
    // as-is and we return the locally-known state.
    if let Some((_, cal_path)) = cache.get_calendar_server_path(&handle.calendar_id)?
        && let Err(e) = refresh_calendar_cache(cache, &handle.nextcloud_account_id, &cal_path).await
    {
        tracing::warn!(
            "RSVP badge: pre-read calendar sync failed (continuing with stale cache): {e}"
        );
    }
    let Some(handle) = cache.get_event_server_handle(&event_id)? else {
        return Ok(None);
    };

    // Build the candidate list — same shape as respond_to_invite.
    let account = load_nextcloud_account(cache, &handle.nextcloud_account_id)?;
    let mut candidates: Vec<String> = Vec::new();
    if let Some(h) = attendee_hint.as_deref() {
        let h = h.trim();
        if !h.is_empty() {
            candidates.push(h.to_string());
        }
    }
    // Profile lookup only exists on a real Nextcloud (#413) — for
    // DAV/local sources the mail-account addresses below carry the
    // identity matching.
    if account.is_nextcloud()
        && let Ok(app_password) = credentials::get_nextcloud_password(&handle.nextcloud_account_id)
        && let Ok(profile) = unkai_nextcloud::user::fetch_current_user(
            &account.server_url,
            &account.username,
            &app_password,
            &account.trusted_certs,
        )
        .await
        && let Some(email) = profile.email
    {
        candidates.push(email);
    }
    if let Ok(mail_accounts) = account_store::load_accounts(cache) {
        for a in mail_accounts {
            candidates.push(a.email);
        }
    }
    let candidates_lc: Vec<String> = candidates.iter().map(|s| s.to_ascii_lowercase()).collect();

    let events = unkai_caldav::ical::parse_ics(&handle.ics_raw)
        .map_err(|e| UnkaiError::Protocol(format!("parse cached event: {e}")))?;
    let partstat = events.into_iter().next().and_then(|event| {
        event.attendees.into_iter().find_map(|att| {
            if candidates_lc.contains(&att.email.to_ascii_lowercase()) {
                att.status.map(|s| s.to_ascii_uppercase())
            } else {
                None
            }
        })
    });
    Ok(partstat)
}

/// `caldav_update_event` with transparent etag-mismatch
/// recovery.  When the cached etag is stale (another client
/// edited the same event between our last sync and this PUT)
/// we sync the parent calendar to pull the new etag, refetch
/// the server handle, and retry the PUT once.  The user never
/// sees the "refresh and try again" failure mode.
///
/// Caller passes the app-side `event_id` so we can refetch
/// the handle after the sync — `event_row_id` is stable across
/// syncs (`{calendar_id}::{uid}`), so the same id resolves to
/// the freshly-synced row with the new etag.
///
/// Returns the (possibly second-attempt) `WriteOutcome` and
/// the handle it was written against.  A second 412 bubbles
/// up unwrapped — that means something else (not a simple
/// stale-cache race) is in conflict, and the caller should
/// surface it.
pub async fn update_event_with_etag_retry(
    cache: &Cache,
    event_id: &str,
    ics: &str,
) -> Result<(unkai_caldav::WriteOutcome, CalendarEventServerHandle), UnkaiError> {
    let handle = load_event_handle(cache, event_id)?;
    let account = load_nextcloud_account(cache, &handle.nextcloud_account_id)?;
    // Local events can't race another client — no etag dance, just
    // mint the next revision (#413).
    if account.is_local() {
        let outcome = unkai_caldav::WriteOutcome {
            href: handle.href.clone(),
            etag: uuid::Uuid::new_v4().to_string(),
        };
        return Ok((outcome, handle));
    }
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;

    match caldav_update_event(
        &handle.href,
        &account.username,
        &app_password,
        &handle.etag,
        ics,
        &account.trusted_certs,
    )
    .await
    {
        Ok(o) => Ok((o, handle)),
        Err(UnkaiError::EtagMismatch(_)) => {
            tracing::info!("stale etag for {event_id}; refreshing calendar cache and retrying");
            let cal_path = cache
                .get_calendar_server_path(&handle.calendar_id)?
                .map(|(_, p)| p)
                .ok_or_else(|| {
                    UnkaiError::Other(format!(
                        "calendar '{}' is not in the local cache",
                        handle.calendar_id
                    ))
                })?;
            refresh_calendar_cache(cache, &handle.nextcloud_account_id, &cal_path).await?;
            let fresh = load_event_handle(cache, event_id)?;
            let outcome = caldav_update_event(
                &fresh.href,
                &account.username,
                &app_password,
                &fresh.etag,
                ics,
                &account.trusted_certs,
            )
            .await?;
            Ok((outcome, fresh))
        }
        Err(e) => Err(e),
    }
}

/// `caldav_delete_event` with the same transparent etag-mismatch
/// recovery the update path uses.  When the cached etag is stale
/// (another client edited the event since our last sync) the
/// PUT comes back as `EtagMismatch` instead of a wordy
/// "refresh and try again" error; we sync the parent calendar,
/// reload the handle with the fresh etag, and retry once.  If
/// the retry comes back 404 (`caldav_delete_event` reports that
/// as `Ok(())` per RFC 4918 §9.6 — the resource is already
/// gone, which is the state we wanted), we surface success too.
///
/// Caller passes the already-loaded `handle` so we don't repeat
/// the cache lookup; in the rare two-step retry case we re-load
/// internally to pick up the fresh href / etag.
pub async fn delete_event_with_etag_retry(
    cache: &Cache,
    event_id: &str,
    handle: &CalendarEventServerHandle,
) -> Result<(), UnkaiError> {
    let nc_account = load_nextcloud_account(cache, &handle.nextcloud_account_id)?;
    // Local events only exist in the cache — the caller's
    // `delete_event_by_id` is the whole delete (#413).
    if nc_account.is_local() {
        return Ok(());
    }
    let app_password = credentials::get_nextcloud_password(&handle.nextcloud_account_id)?;

    match caldav_delete_event(
        &handle.href,
        &nc_account.username,
        &app_password,
        &handle.etag,
        &nc_account.trusted_certs,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(UnkaiError::EtagMismatch(_)) => {
            tracing::info!("stale etag for delete of {event_id}; refreshing calendar and retrying");
            let cal_path = cache
                .get_calendar_server_path(&handle.calendar_id)?
                .map(|(_, p)| p)
                .ok_or_else(|| {
                    UnkaiError::Other(format!(
                        "calendar '{}' is not in the local cache",
                        handle.calendar_id
                    ))
                })?;
            refresh_calendar_cache(cache, &handle.nextcloud_account_id, &cal_path).await?;
            // Refresh may have removed the row entirely (someone
            // else already deleted the event).  Treat that as
            // success — our intent was "make this event go
            // away", which is now true.
            let Some(fresh) = cache
                .get_event_server_handle(event_id)
                .map_err(UnkaiError::from)?
            else {
                return Ok(());
            };
            caldav_delete_event(
                &fresh.href,
                &nc_account.username,
                &app_password,
                &fresh.etag,
                &nc_account.trusted_certs,
            )
            .await
        }
        Err(e) => Err(e),
    }
}

/// Pull the latest events for one calendar via CalDAV
/// sync-collection and apply the delta to the local cache.
/// Same plumbing as `sync_nextcloud_calendars`'s inner loop, but
/// scoped to a single calendar so the inbound-RSVP path can
/// freshen its cache before deciding create-vs-update.  Soft
/// failures (server transient, no auth, anything) bubble back as
/// `Err`; the caller decides whether to fall through.
pub async fn refresh_calendar_cache(
    cache: &Cache,
    nc_id: &str,
    calendar_path: &str,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(cache, nc_id)?;
    // Nothing to refresh for a local-only source (#413) — the cache
    // is already the freshest state there is.
    if account.is_local() {
        return Ok(());
    }
    let app_password = credentials::get_nextcloud_password(nc_id)?;
    // Look up the local calendar id by path so we can fetch its
    // sync token and apply the delta against it.
    let calendars = cache.list_calendars(nc_id)?;
    let cal = calendars
        .into_iter()
        .find(|c| c.path == calendar_path)
        .ok_or_else(|| {
            UnkaiError::Other(format!(
                "calendar '{calendar_path}' is not in the local cache"
            ))
        })?;
    let prev_token = cache
        .get_calendar_sync_state(&cal.id)
        .ok()
        .flatten()
        .and_then(|s| s.sync_token);
    let delta = caldav_sync_calendar(
        &url_origin(&cal.path),
        &cal.path,
        &account.username,
        &app_password,
        prev_token.as_deref(),
        &account.trusted_certs,
    )
    .await?;
    let upserts: Vec<CalendarEventRow> = delta.upserts.iter().flat_map(raw_event_to_rows).collect();
    cache.apply_event_delta(
        &cal.id,
        &upserts,
        &delta.deleted_hrefs,
        delta.new_sync_token.as_deref(),
        cal.ctag.as_deref(),
    )?;
    Ok(())
}

pub fn load_event_handle(
    cache: &Cache,
    event_id: &str,
) -> Result<CalendarEventServerHandle, UnkaiError> {
    cache
        .get_event_server_handle(event_id)
        .map_err(UnkaiError::from)?
        .ok_or_else(|| {
            UnkaiError::Other(format!(
                "event '{event_id}' is not in the local cache — refresh and try again"
            ))
        })
}

/// Flatten one CalDAV resource (href-with-ics) into one store row per
/// VEVENT it contains. Master + recurrence-id overrides all share the
/// same `href`, `etag`, and `ics_raw` — `apply_event_delta` keys the
/// wipe-on-upsert by href, so re-syncing an href with fewer overrides
/// correctly removes the ones that vanished server-side.
pub fn raw_event_to_rows(raw: &RawEvent) -> Vec<CalendarEventRow> {
    raw.events
        .iter()
        .map(|e| CalendarEventRow {
            // The caldav parser stores the VEVENT UID in `id`.
            uid: e.id.clone(),
            recurrence_id: e.recurrence_id,
            href: raw.href.clone(),
            etag: raw.etag.clone(),
            summary: e.summary.clone(),
            description: e.description.clone(),
            start: e.start,
            end: e.end,
            location: e.location.clone(),
            rrule: e.rrule.clone(),
            rdate: e.rdate.clone(),
            exdate: e.exdate.clone(),
            url: e.url.clone(),
            transparency: e.transparency.clone(),
            attendees: e.attendees.clone(),
            reminders: e.reminders.clone(),
            latitude: e.latitude,
            longitude: e.longitude,
            ics_raw: raw.ics_raw.clone(),
        })
        .collect()
}

/// Find an iCalendar payload anywhere in the message and return
/// its raw bytes.  Used by MailView as a fallback for invites
/// where the cached `attachments` array doesn't surface the
/// calendar — most commonly the canonical iMIP MIME shape
/// where `text/calendar` is a body alternative inside
/// `multipart/alternative` and mail-parser classifies it as a
/// body part rather than an attachment.  Returns `None` when
/// the message genuinely has no calendar content (caller hides
/// the RSVP card).
pub async fn download_calendar_from_message(
    account_id: String,
    folder: String,
    uid: u32,
    cache: &Cache,
) -> Result<Option<Vec<u8>>, UnkaiError> {
    let account = load_account(cache, &account_id)?;
    if uses_jmap(&account) {
        return Err(UnkaiError::Protocol(
            "JMAP calendar extraction is not implemented yet".into(),
        ));
    }
    let mut client = connect_imap(&account).await?;
    let bytes = client.fetch_calendar_payload(&folder, uid).await?;
    let _ = client.logout().await;
    Ok(bytes)
}

/// Suppress further reminders for the given UID until the user
/// reopens the editor or the in-memory state is reset (process
/// restart).  Called from JS when the user clicks Dismiss on
/// the reminder popup or joins a meeting early so we don't
/// pester them mid-event.
pub fn dismiss_event_reminder(uid: String, state: &EventReminderState) -> Result<(), UnkaiError> {
    {
        let mut d = state
            .dismissed
            .lock()
            .expect("event-reminder dismissed mutex");
        d.insert(uid.clone());
    }
    {
        let mut f = state.fired.lock().expect("event-reminder fired mutex");
        f.retain(|(u, _)| u != &uid);
    }
    {
        // Snooze and dismiss are mutually exclusive — clear any
        // pending snooze on the same UID so it doesn't fire after
        // the user has already dismissed the event entirely.
        let mut s = state.snoozes.lock().expect("event-reminder snoozes mutex");
        s.remove(&uid);
    }
    Ok(())
}

/// Schedule a re-fire for the given UID at `snooze_until_iso`
/// (RFC 3339 / ISO 8601 in UTC).  Called from JS when the user
/// picks a "Remind me in …" option on the reminder popup.
///
/// While a snooze is pending the scanner skips the event's
/// normal VALARM-driven reminders (so the user doesn't get
/// double-toasted from both sources).  Once `now` crosses the
/// snooze moment the next scan tick fires a synthetic reminder
/// and removes the entry.
pub fn snooze_event_reminder(
    uid: String,
    snooze_until_iso: String,
    state: &EventReminderState,
) -> Result<(), UnkaiError> {
    let snooze_until = chrono::DateTime::parse_from_rfc3339(&snooze_until_iso)
        .map_err(|e| {
            UnkaiError::Other(format!(
                "snooze_event_reminder: invalid timestamp '{snooze_until_iso}': {e}"
            ))
        })?
        .with_timezone(&chrono::Utc);
    {
        let mut s = state.snoozes.lock().expect("event-reminder snoozes mutex");
        s.insert(uid.clone(), snooze_until);
    }
    {
        // Drop any stale `fired` entry so the scanner is willing
        // to re-fire when the snooze elapses.  Without this the
        // dedupe key `(uid, minutes_before)` would block the
        // post-snooze synthetic reminder.
        let mut f = state.fired.lock().expect("event-reminder fired mutex");
        f.retain(|(u, _)| u != &uid);
    }
    Ok(())
}

/// Read an `.ics` file from disk and parse it into one or more
/// `CalendarEvent`s.  Caller (the import-from-disk flow) opens the
/// first event in the EventEditor so the user can pick a target
/// calendar and save it via the existing create path.
pub fn parse_ics_file(path: String) -> Result<Vec<unkai_core::models::CalendarEvent>, UnkaiError> {
    let body = std::fs::read_to_string(&path)
        .map_err(|e| UnkaiError::Other(format!("read {path}: {e}")))?;
    unkai_caldav::ical::parse_ics(&body)
}

/// Summary of a completed calendar import (#518). Same shape as the
/// contacts `ImportContactsReport`: a partial-failure-prone batch
/// operation returns counts plus per-entry reasons instead of
/// failing wholesale on the first bad VEVENT.
#[derive(Debug, Clone, Serialize)]
pub struct ImportCalendarReport {
    /// VEVENTs found in the file, before dedup and write attempts.
    pub total: u32,
    /// Events actually created in the target calendar.
    pub imported: u32,
    /// Entries skipped because an event with the same iCalendar UID
    /// already exists on one of the user's calendars.
    pub skipped_duplicates: u32,
    /// Human-readable reasons for entries that could not be imported
    /// (recurrence exceptions, per-event write failures).
    pub errors: Vec<String>,
}

/// Import every VEVENT from an `.ics` file into one calendar (#518).
///
/// Each event funnels through the same write path as the create
/// form — `build_ics` → [`dav_create_event_for`] (which
/// short-circuits for local-only sources) → cache upsert — so an
/// imported event is indistinguishable from a hand-created one.
///
/// Two deliberate departures from a byte-faithful copy:
///
/// - **The file's UID is kept**, not re-minted. That makes dedup
///   possible (re-importing the same export is a no-op) and keeps
///   the event's identity for any later iMIP traffic about it.
///   Dedup checks the whole account, not just the target calendar —
///   the same event living on a sibling calendar is still a
///   duplicate.
/// - **ATTENDEE lines are dropped.** The create path stamps the
///   current user as ORGANIZER (Sabre rejects attendees without
///   one), and an ORGANIZER matching the user's principal makes
///   Nextcloud's iTIP broker mail an invitation to every attendee —
///   importing an old calendar export must never spray invites at
///   hundreds of past participants. Events land as plain copies;
///   reminders, recurrence, and everything else survive.
///
/// Recurrence *exceptions* (VEVENTs carrying `RECURRENCE-ID`) are
/// skipped with a per-entry note: CalDAV requires master and
/// overrides to share one resource, and the single-VEVENT
/// `build_ics` writer can't express that. The master series still
/// imports, so only the individually-modified occurrences revert
/// to the series shape.
pub async fn import_calendar_file(
    calendar_id: String,
    path: String,
    cache: &Cache,
    ui: &dyn UiNotifier,
) -> Result<ImportCalendarReport, UnkaiError> {
    let (nc_id, calendar_path) =
        cache
            .get_calendar_server_path(&calendar_id)?
            .ok_or_else(|| {
                UnkaiError::Other(format!(
                    "calendar '{calendar_id}' is not in the local cache — refresh and try again"
                ))
            })?;
    let account = load_nextcloud_account(cache, &nc_id)?;

    let body = std::fs::read_to_string(&path)
        .map_err(|e| UnkaiError::Other(format!("read {path}: {e}")))?;
    let events = unkai_caldav::ical::parse_ics(&body)?;

    let mut report = ImportCalendarReport {
        total: events.len() as u32,
        imported: 0,
        skipped_duplicates: 0,
        errors: Vec::new(),
    };

    // UIDs written during this run — a file listing the same UID
    // twice imports it once instead of erroring on the second PUT.
    let mut seen_uids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for mut event in events {
        let label = if event.summary.is_empty() {
            event.id.clone()
        } else {
            event.summary.clone()
        };
        if event.recurrence_id.is_some() {
            report.errors.push(format!(
                "{label}: modified occurrence of a recurring series skipped"
            ));
            continue;
        }
        let uid = event.id.clone();
        if seen_uids.contains(&uid) || cache.find_event_id_by_uid(&uid)?.is_some() {
            report.skipped_duplicates += 1;
            continue;
        }

        event.attendees.clear();
        let ics = caldav_build_ics(&event, None, None);
        match dav_create_event_for(&account, &calendar_path, &uid, &ics).await {
            Ok(outcome) => {
                let row = calendar_event_to_row(&event, &outcome.href, &outcome.etag, &ics);
                if let Err(e) = cache.upsert_single_event(&calendar_id, &row) {
                    report.errors.push(format!("{label}: {e}"));
                    continue;
                }
                report.imported += 1;
                seen_uids.insert(uid);
            }
            Err(e) => {
                flag_calendar_read_only_on_forbidden(ui, cache, &calendar_id, &e);
                // A calendar flipped read-only mid-import fails the
                // same way for every remaining event — stop early
                // with one clear reason instead of N copies.
                if matches!(e, UnkaiError::CalDavWriteForbidden(_)) {
                    report.errors.push(format!("{label}: {e}"));
                    break;
                }
                report.errors.push(format!("{label}: {e}"));
            }
        }
    }

    if report.imported > 0 {
        ui.calendars_updated(&CalendarsUpdatedPayload {
            nextcloud_account_id: Some(nc_id),
        });
    }
    Ok(report)
}

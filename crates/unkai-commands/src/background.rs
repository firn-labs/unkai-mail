//! The long-running loops the shell spawns at startup (#476).
//!
//! Each one takes an [`AppContext`](crate::state::AppContext) and runs
//! until the process exits: mail polling, event / message reminders,
//! outbox draining, settings sync, and the URLhaus refresh.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use unkai_core::UnkaiError;
use unkai_core::models::CalendarEvent;
use unkai_store::Cache;
use unkai_store::account_store;
use unkai_store::credentials;
use unkai_store::link_check;
use unkai_store::nextcloud_store;
use unkai_store::settings_bundle;
use unkai_store::settings_sync;

use crate::compose::try_drain_outbox_entry;
use crate::mail::{check_mail_now_inner, fetch_message_inner, refresh_urlhaus_inner};
use crate::notify::{EventReminderPayload, MessageReminderPayload};
use crate::settings::{UNKAI_SETTINGS_DIR, UNKAI_SETTINGS_FILE};
use crate::state::AppContext;
use crate::state::{SharedLocalStorage, SharedSettings};
use crate::system::UNKAI_TEMP_ROOT;

/// Minimum enforced sync interval — guards against a hand-edited
/// `app_settings.json` DOSing the user's mail server.
pub const MIN_SYNC_INTERVAL_SECS: u64 = 30;

// ── Talk-join reminders (issue #123) ──────────────────────────
//
// Goal: fire a desktop notification ahead of any calendar event
// whose VALARM lead time has just elapsed (issues #123 + #203).
// Lead time is taken from the event's own `VALARM` reminders so
// the user controls timing per-event.  Rides the background sync
// loop's tick, so no extra timers; in-memory dedupe keys off
// `(uid, minutes_before)` so a second tick within the firing
// window doesn't double-toast.
//
// Two settings flags gate the scanner per event:
//   * `meeting_reminders_enabled` — for events that carry a
//     meeting URL (Talk / Zoom / Meet / Teams / Jitsi / …).
//   * `calendar_reminders_enabled` — for events without one.
// Keeping them separate lets users mute one stream without
// silencing the other (e.g. "remind me about meetings but
// don't nag me about every event with an alarm").

/// Lead time in seconds we'll widen the firing window by, on
/// each side of the reminder's exact moment.  Slightly larger
/// than the default 60s tick so a tick that drifts by a few
/// seconds doesn't miss the reminder entirely.
pub const EVENT_REMINDER_FIRE_TOLERANCE_SECS: i64 = 90;

/// Pull the first plausible meeting URL out of an event's body
/// text — Nextcloud Talk, Zoom, Teams, Google Meet, Webex, Jitsi,
/// etc.  Any HTTP(S) URL counts; we don't try to be smart about
/// which platform it points at because that ages badly (every
/// quarter brings a new conferencing service).
///
/// Searched fields, in priority order: `URL` (canonical), then
/// `LOCATION` (a common place for join links), then
/// `DESCRIPTION` (where pasted "click to join" links land).
pub fn extract_meeting_url(event: &CalendarEvent) -> Option<String> {
    fn extract_from(s: &str) -> Option<String> {
        // Walk word by word so the trailing punctuation in
        // pasted plain-text bodies ("…click here: <url>.")
        // doesn't end up baked into the captured URL.
        for token in s.split_whitespace() {
            let url = token.trim_matches(|c: char| {
                c == '<'
                    || c == '>'
                    || c == '"'
                    || c == '\''
                    || c == ','
                    || c == '.'
                    || c == ';'
                    || c == ')'
                    || c == '('
            });
            if url.starts_with("http://") || url.starts_with("https://") {
                return Some(url.to_string());
            }
        }
        None
    }
    let url_field = event.url.as_deref().unwrap_or("");
    let loc_field = event.location.as_deref().unwrap_or("");
    let desc_field = event.description.as_deref().unwrap_or("");
    extract_from(url_field)
        .or_else(|| extract_from(loc_field))
        .or_else(|| extract_from(desc_field))
}

/// Scan upcoming events for ones whose VALARM lead time we've
/// just reached, and emit an `event-reminder` event for each
/// match (gated per-event by the user's two reminder settings —
/// `meeting_reminders_enabled` for events with a meeting URL,
/// `calendar_reminders_enabled` for events without).  Called
/// from the background sync loop; cheap because it reads from
/// the local cache only.
pub async fn check_event_reminders_inner(ctx: &AppContext) -> Result<(), UnkaiError> {
    use chrono::Utc;

    let settings = &ctx.settings;
    let (meetings_on, calendar_on) = {
        let s = settings.read().await;
        (s.meeting_reminders_enabled, s.calendar_reminders_enabled)
    };
    if !meetings_on && !calendar_on {
        return Ok(());
    }

    // Build the list of calendars whose events should trigger a
    // reminder: every non-hidden, non-muted calendar across every
    // connected NC account.  Mirrors the visibility the user
    // already chose for the agenda grid; muting a calendar there
    // also silences its Talk reminders.
    let cache = &ctx.cache;
    let nc_accounts = nextcloud_store::load_accounts(cache).unwrap_or_default();
    let mut calendar_ids: Vec<String> = Vec::new();
    for acc in &nc_accounts {
        if let Ok(list) = cache.list_calendars(&acc.id) {
            for c in list {
                if !c.hidden && !c.muted {
                    calendar_ids.push(c.id);
                }
            }
        }
    }
    if calendar_ids.is_empty() {
        return Ok(());
    }

    // Window: from now back ~tolerance (so a tick that just
    // crossed the reminder time still catches it) forward 7 days
    // (covers reminders up to "1 week before", the largest
    // preset the editor offers — #236).  An event whose 1-week
    // reminder is approaching has its `start` 7 days from now,
    // so the cache filter must include events that far ahead or
    // the reminder never fires.  Cheap: same per-calendar
    // expansion path the agenda grid already runs, just with a
    // wider date range.
    let now = Utc::now();
    let tolerance = chrono::Duration::seconds(EVENT_REMINDER_FIRE_TOLERANCE_SECS);
    let range_start = now - tolerance;
    let range_end = now + chrono::Duration::days(7) + tolerance;

    let input = match cache.list_events_for_expansion(&calendar_ids, range_start, range_end) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("talk-reminder scan: list_events_for_expansion failed: {e}");
            return Ok(());
        }
    };

    // Re-run the same RRULE expansion the agenda grid uses so
    // the recurring-event case is handled once, here, instead of
    // duplicated.
    let mut overrides_by_master: std::collections::HashMap<&str, Vec<&CalendarEvent>> =
        std::collections::HashMap::new();
    for ov in &input.overrides {
        if let Some(master_id) = ov.id.rsplit_once("::").map(|(prefix, _)| prefix) {
            overrides_by_master.entry(master_id).or_default().push(ov);
        }
    }
    let mut events: Vec<CalendarEvent> = input.singletons;
    for master in &input.masters {
        let ovs = overrides_by_master
            .get(master.id.as_str())
            .cloned()
            .unwrap_or_default();
        events.extend(unkai_caldav::expand_event(
            master,
            &ovs,
            range_start,
            range_end,
        ));
    }

    let state = &ctx.reminders;
    {
        // Prune `fired` entries whose event has already started —
        // keeps the set bounded in long-running sessions and
        // ensures a meeting that recurs daily fires its reminder
        // again on the next occurrence.
        let mut fired = state.fired.lock().expect("event-reminder fired mutex");
        let active_uids: HashSet<String> = events
            .iter()
            .filter(|e| e.start > now)
            .map(|e| vevent_uid_from_event_id(&e.id))
            .collect();
        fired.retain(|(uid, _)| active_uids.contains(uid));
    }
    let dismissed_snapshot: HashSet<String> = {
        let d = state
            .dismissed
            .lock()
            .expect("event-reminder dismissed mutex");
        d.clone()
    };
    // Snapshot the snooze map so we can read without holding the
    // lock through the loop — and a separate list of snooze
    // entries to fire & evict at the end of the scan.
    let snoozes_snapshot: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> = {
        let s = state.snoozes.lock().expect("event-reminder snoozes mutex");
        s.clone()
    };
    let mut snoozes_to_evict: Vec<String> = Vec::new();

    for ev in &events {
        // Skip events whose start is far enough in the past that
        // even a 0-min reminder would no longer be inside the
        // per-reminder fire-tolerance window.  Using the same
        // `EVENT_REMINDER_FIRE_TOLERANCE_SECS` constant the
        // per-reminder check uses (vs. the previous hard-coded
        // 1-minute) means the "At event start" preset now has
        // the full tolerance window to fire — without this,
        // a scan tick landing 60–90 s after the event start
        // would silently drop the 0-min reminder even though
        // the per-reminder check would have accepted it.
        if ev.start <= now - chrono::Duration::seconds(EVENT_REMINDER_FIRE_TOLERANCE_SECS) {
            continue;
        }
        let meeting_url = extract_meeting_url(ev);
        // Per-event gate.  Events with a meeting URL ride the
        // `meeting_reminders_enabled` flag; everything else
        // rides `calendar_reminders_enabled`.  Either flag being
        // off silences just that bucket.
        let gate_open = if meeting_url.is_some() {
            meetings_on
        } else {
            calendar_on
        };
        if !gate_open {
            continue;
        }
        let uid = vevent_uid_from_event_id(&ev.id);
        if dismissed_snapshot.contains(&uid) {
            continue;
        }

        // ── Snooze path ───────────────────────────────────────
        // If the user picked "Remind me 5 min before" / etc. on
        // the popup, the dispatch table tells us the next time
        // to fire for this UID.  We *bypass* the VALARM-driven
        // path entirely while a snooze is pending so we don't
        // double-fire from both sources, then re-fire here when
        // `now` crosses the snooze moment.
        if let Some(snooze_until) = snoozes_snapshot.get(&uid) {
            if now < *snooze_until {
                // Still snoozed — skip everything else for this event.
                continue;
            }
            // Snooze elapsed — fire a synthetic reminder with the
            // matching minutes_before label, then evict the entry.
            let minutes_before =
                ((ev.start - now).num_seconds().max(0) / 60).clamp(0, i32::MAX as i64) as i32;
            let payload = EventReminderPayload {
                event_id: ev.id.clone(),
                uid: uid.clone(),
                summary: ev.summary.clone(),
                start: ev.start,
                end: ev.end,
                location: ev.location.clone(),
                attendees: ev.attendees.iter().map(|a| a.email.clone()).collect(),
                meeting_url: meeting_url.clone(),
                minutes_before,
            };
            {
                ctx.ui.event_reminder(&payload);
                tracing::info!(
                    "event-reminder fired (post-snooze): uid={} ({} min before)",
                    uid,
                    minutes_before
                );
            }
            snoozes_to_evict.push(uid.clone());
            // Don't also walk the VALARM-driven path for this event
            // on the same scan — the snooze fire stands in for it.
            continue;
        }

        if ev.reminders.is_empty() {
            // No VALARM on the event → user didn't ask for a
            // reminder; respect that.
            continue;
        }

        for reminder in &ev.reminders {
            let minutes = reminder.trigger_minutes_before;
            // Negative `minutes_before` means "after start" — out
            // of scope for a join reminder, skip silently.
            if minutes < 0 {
                continue;
            }
            let fire_at = ev.start - chrono::Duration::minutes(minutes as i64);
            // Fire when `now` is in [fire_at, fire_at + tolerance]:
            // we never look earlier than the requested moment, but
            // do allow a tick's worth of catch-up so a slightly
            // late tick still lands.
            let elapsed = (now - fire_at).num_seconds();
            if !(0..=EVENT_REMINDER_FIRE_TOLERANCE_SECS).contains(&elapsed) {
                continue;
            }

            let key = (uid.clone(), minutes);
            {
                let mut fired = state.fired.lock().expect("event-reminder fired mutex");
                if fired.contains(&key) {
                    continue;
                }
                fired.insert(key);
            }

            let payload = EventReminderPayload {
                event_id: ev.id.clone(),
                uid: uid.clone(),
                summary: ev.summary.clone(),
                start: ev.start,
                end: ev.end,
                location: ev.location.clone(),
                attendees: ev.attendees.iter().map(|a| a.email.clone()).collect(),
                meeting_url: meeting_url.clone(),
                minutes_before: minutes,
            };
            {
                ctx.ui.event_reminder(&payload);
                tracing::info!(
                    "event-reminder fired: uid={} ({} min before, meeting={})",
                    uid,
                    minutes,
                    meeting_url.is_some()
                );
            }
        }
    }

    // Evict snoozes we just fired so we don't loop on them
    // forever.  Done after the read loop so we never hold the
    // snoozes mutex through the per-event work.
    if !snoozes_to_evict.is_empty() {
        let mut s = state.snoozes.lock().expect("event-reminder snoozes mutex");
        for uid in &snoozes_to_evict {
            s.remove(uid);
        }
    }

    Ok(())
}

/// Recover the bare VEVENT UID from a composite cached id —
/// `{nc_id}::{cal_path}::{uid}` for masters/singletons or
/// `{nc_id}::{cal_path}::{uid}::occ::{epoch}` for expanded
/// occurrences.  The frontend's `dismiss_event_reminder` and the
/// dedupe set both key off the bare UID so all occurrences of
/// the same series share a single dismiss / fire entry.
pub fn vevent_uid_from_event_id(id: &str) -> String {
    let parts: Vec<&str> = id.split("::").collect();
    if parts.len() >= 3 {
        parts[2].to_string()
    } else {
        id.to_string()
    }
}

// ── Message reminders (#415) ────────────────────────────────────
//
// Unlike calendar reminders — whose fire times derive from VALARMs
// on cached events — a message reminder is a single user-chosen
// moment persisted straight into the `messages.reminder_at` column.
// That gives restart-survival for free: the scanner asks the DB
// "anything elapsed?" on every tick, so a reminder that came due
// while the app was closed fires (late) on the first tick after
// the next launch instead of being lost.  No in-memory dedupe
// state is needed either — firing *clears the column*, which is
// the dedupe.

/// Tick length for the message-reminder scanner.  Deliberately its
/// own loop rather than riding `background_sync_loop`: the sync
/// interval is user-configurable (and sync can be disabled
/// entirely), but a reminder the user explicitly set should fire
/// within moments of its chosen time regardless of how mail
/// polling is configured.  The scan is one indexed SQL query, so a
/// short fixed tick costs effectively nothing.
pub const MESSAGE_REMINDER_TICK_SECS: u64 = 30;

/// One scan: emit a `message-reminder` event for every elapsed
/// reminder, then clear each fired row's `reminder_at`.  Clearing
/// only happens after a successful emit — if the emit fails the
/// row stays put and the next tick retries, which errs on the
/// side of a duplicate toast over a silently lost reminder.
pub async fn check_message_reminders_inner(ctx: &AppContext) -> Result<(), UnkaiError> {
    let now = chrono::Utc::now().timestamp();
    let due = {
        let cache = &ctx.cache;
        cache
            .due_message_reminders(now)
            .map_err(|e| UnkaiError::Other(format!("due_message_reminders failed: {e}")))?
    };
    for r in due {
        let payload = MessageReminderPayload {
            account_id: r.account_id.clone(),
            folder: r.folder.clone(),
            uid: r.uid,
            from: r.from,
            subject: r.subject,
        };
        if let Err(e) = ctx.ui.message_reminder(&payload) {
            tracing::warn!("failed to emit message-reminder event: {e}");
            continue;
        }
        let cache = &ctx.cache;
        if let Err(e) = cache.set_message_reminder(&r.account_id, &r.folder, r.uid, None) {
            tracing::warn!(
                "failed to clear fired reminder for {}/{}/{}: {e}",
                r.account_id,
                r.folder,
                r.uid,
            );
        }
    }
    Ok(())
}

/// Dedicated fixed-cadence loop for message reminders.  Spawned
/// once at setup, next to `background_sync_loop`.
pub async fn message_reminder_loop(ctx: AppContext) {
    tracing::info!("message reminder loop started");
    loop {
        tokio::time::sleep(Duration::from_secs(MESSAGE_REMINDER_TICK_SECS)).await;
        if let Err(e) = check_message_reminders_inner(&ctx).await {
            tracing::warn!("check_message_reminders_inner failed: {e}");
        }
    }
}

/// Launch-time message-body prerender (#178).
///
/// For every configured account, fetch the bodies of the newest INBOX
/// envelopes that don't yet have a cached body.  The user clicking
/// any of those messages then reads from disk instead of paying for
/// an IMAP round-trip — eliminates the "open mail → blank pane →
/// content appears" beat on a fresh launch.
///
/// Bounded to `PRERENDER_LIMIT` per account so a brand-new install
/// (every envelope missing a body) doesn't drown the launch in
/// FETCHes.  Accounts run concurrently; within an account we go
/// sequentially because each `fetch_message_inner` opens its own
/// IMAP connection and we don't want N parallel auths against the
/// same server.
pub async fn prerender_inboxes_on_launch(ctx: &AppContext) {
    /// Ten messages per account is a sweet spot — covers the
    /// usually-visible top of the inbox without ballooning the
    /// launch into a body-sync.  Tuning knob if real-world usage
    /// suggests otherwise.
    const PRERENDER_LIMIT: u32 = 10;

    let cache = &ctx.cache;
    let accounts = account_store::load_accounts(cache).unwrap_or_default();

    let mut handles = Vec::new();
    for account in accounts {
        let cache = ctx.cache.clone();
        handles.push(tokio::spawn(async move {
            let uids = match cache.get_envelopes_missing_body(&account.id, "INBOX", PRERENDER_LIMIT)
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "prerender: failed to list missing bodies for '{}': {e}",
                        account.id,
                    );
                    return;
                }
            };
            if uids.is_empty() {
                return;
            }
            tracing::info!(
                "prerender: warming {} message body/bodies for '{}'",
                uids.len(),
                account.id,
            );
            for uid in uids {
                if let Err(e) = fetch_message_inner(&account.id, "INBOX", uid, &cache).await {
                    tracing::debug!(
                        "prerender: fetch_message_inner({}, INBOX, {uid}) failed: {e}",
                        account.id,
                    );
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

/// Periodic poll. Re-reads the settings snapshot each tick so the user
/// can toggle sync on/off or change the interval and have it take
/// effect on the next cycle without restarting the loop.
pub async fn background_sync_loop(ctx: AppContext) {
    tracing::info!("background sync loop started");
    loop {
        let (enabled, interval) = {
            let s = ctx.settings.read().await;
            (
                s.background_sync_enabled,
                Duration::from_secs(s.background_sync_interval_secs.max(MIN_SYNC_INTERVAL_SECS)),
            )
        };

        tokio::time::sleep(interval).await;

        if !enabled {
            continue;
        }
        if let Err(e) = check_mail_now_inner(&ctx).await {
            tracing::warn!("background check_mail_now_inner failed: {e}");
        }
        // Event reminders ride the same tick — the cache is
        // already warm from the mail poll above and the scan is
        // a couple of SQL queries plus an in-memory loop.
        if let Err(e) = check_event_reminders_inner(&ctx).await {
            tracing::warn!("background check_event_reminders_inner failed: {e}");
        }
        // #276: drain the Outbox.  Walks every queued row across
        // every account and re-attempts the SMTP send.  No-op
        // when the queue is empty (one COUNT(*) check before any
        // network work), so a healthy install pays only the cost
        // of that aggregate per tick.
        drain_outbox_sweep(&ctx).await;
    }
}

/// Periodic drain pass over `outbox_messages`.  Called from the
/// `background_sync_loop` on every sync tick.  Each row goes
/// through `try_drain_outbox_entry` — same code the
/// `send_email`-spawned task and the manual-retry command use,
/// so a row eventually drains via whichever path completes
/// first.  Done sequentially to keep concurrent SMTP connections
/// to one per account; even a large queue (dozens of rows) is
/// finished well within a sync interval on a healthy network.
pub async fn drain_outbox_sweep(ctx: &AppContext) {
    let cache_state = &ctx.cache;
    let rows = match cache_state.list_all_outbox() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("list_all_outbox during drain sweep failed: {e}");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    tracing::info!("outbox drain sweep: {} queued row(s)", rows.len());
    for row in rows {
        try_drain_outbox_entry(ctx.ui.as_ref(), cache_state, row.id).await;
    }
}

/// One push attempt.  Best-effort folder creation, then PUT.
/// Folder creates are intentionally swallowed because
/// `create_directory` returns `UnkaiError::Nextcloud` for the
/// idempotent "folder already exists" case — it's not actually
/// an error from our perspective.
pub async fn push_settings_to_nc(
    cache: &Cache,
    app_settings_file: &std::path::Path,
    local_storage: std::collections::HashMap<String, String>,
    nc_id: &str,
) -> Result<(), UnkaiError> {
    let bundle = settings_bundle::build_bundle(cache, app_settings_file, local_storage)?;
    let json = settings_bundle::serialise(&bundle)?;

    let account = nextcloud_store::load_accounts(cache)?
        .into_iter()
        .find(|a| a.id == nc_id)
        .ok_or_else(|| UnkaiError::Other(format!("Nextcloud account '{nc_id}' not found")))?;
    let app_password = credentials::get_nextcloud_password(nc_id)?;

    // Idempotent folder creates.  The Office viewer code already
    // ensures `/Unkai Mail` for the temp area, but a user who
    // hasn't opened any Office attachments won't have triggered
    // that path yet — we make sure both rungs of the hierarchy
    // exist before the PUT.
    for dir in [UNKAI_TEMP_ROOT, UNKAI_SETTINGS_DIR] {
        if let Err(e) = unkai_nextcloud::create_directory(
            &account.server_url,
            &account.username,
            &app_password,
            dir,
            &account.trusted_certs,
        )
        .await
        {
            // 405 / "already exists" is the happy path; only the
            // network/auth/quota classes need to bubble up.
            let msg = e.to_string();
            if !msg.contains("already") && !msg.contains("405") && !msg.contains("HTTP 405") {
                return Err(e);
            }
        }
    }

    unkai_nextcloud::upload_file(
        &account.server_url,
        &account.username,
        &app_password,
        UNKAI_SETTINGS_FILE,
        json.into_bytes(),
        Some("application/json"),
        &account.trusted_certs,
    )
    .await?;
    Ok(())
}

/// Auto-sync worker.  Wakes on either a `notify_one()` from a
/// settings-changed event or a 5-minute periodic tick (the retry
/// path for "user changed a setting while offline and never
/// changed another"), and pushes the bundle to the configured NC
/// account if one is set.  Failures keep `pending=true` so the
/// next opportunity tries again.
pub async fn settings_sync_worker(
    ctx: AppContext,
    local_storage: SharedLocalStorage,
    notify: Arc<tokio::sync::Notify>,
) {
    use tokio::time::{Duration, MissedTickBehavior, interval, sleep};

    // The worker reads/writes its profile's settings_sync.json for
    // its whole lifetime (#531/#533) — the profile rides in on the
    // context, like every other loop.
    let cache = ctx.cache.clone();
    let sync_file = ctx.profile.settings_sync_file();

    let mut retry_tick = interval(Duration::from_secs(300));
    retry_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The first `tick()` returns immediately — burn it so the
    // periodic path doesn't fire on launch.  The launch-time
    // recovery happens via the explicit `notify_one()` call from
    // `main()` instead.
    retry_tick.tick().await;

    loop {
        tokio::select! {
            _ = notify.notified() => {
                // Debounce: a burst of changes (e.g. dragging
                // the UI scale slider) coalesces into one push.
                sleep(Duration::from_secs(2)).await;
            }
            _ = retry_tick.tick() => {
                // Periodic retry — only meaningful if we have
                // something to flush, so peek the disk state.
                let state = settings_sync::load_state(&sync_file).unwrap_or_default();
                if !state.pending || state.target_nc_id.is_none() {
                    continue;
                }
            }
        }

        // Read the disk state fresh; the user may have flipped
        // the toggle off between the wake and now.
        let state = match settings_sync::load_state(&sync_file) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("settings_sync load_state failed: {e}");
                continue;
            }
        };
        let Some(target) = state.target_nc_id.clone() else {
            // Sync turned off — clear any stale pending flag so
            // a re-enable doesn't immediately fire a stale push.
            if state.pending {
                let _ = settings_sync::save_state(
                    &sync_file,
                    &settings_sync::SettingsSyncState {
                        target_nc_id: None,
                        pending: false,
                    },
                );
            }
            continue;
        };

        let snapshot = local_storage.read().await.clone();
        match push_settings_to_nc(&cache, &ctx.profile.app_settings_file(), snapshot, &target).await
        {
            Ok(()) => {
                tracing::info!("Settings bundle synced to Nextcloud '{target}'");
                if state.pending {
                    let _ = settings_sync::save_state(
                        &sync_file,
                        &settings_sync::SettingsSyncState {
                            target_nc_id: state.target_nc_id,
                            pending: false,
                        },
                    );
                }
            }
            Err(e) => {
                // Silent in the UI; warn-level in the log so a
                // developer chasing "why isn't my NC backup
                // updating" can see what went wrong.
                tracing::warn!("Settings sync to '{target}' failed (will retry later): {e}");
                if !state.pending {
                    let _ = settings_sync::save_state(
                        &sync_file,
                        &settings_sync::SettingsSyncState {
                            target_nc_id: state.target_nc_id,
                            pending: true,
                        },
                    );
                }
            }
        }
    }
}

/// Background refresh worker.  Driven by an hourly tick plus a
/// startup-time decision: if the local snapshot is empty or
/// older than 24 h, refresh immediately; otherwise wait.  The
/// worker respects the `link_check_enabled` master toggle —
/// when off, it sleeps for the full tick window and re-checks
/// before doing any network work.
pub async fn urlhaus_refresh_worker(cache: Cache, settings: SharedSettings) {
    use tokio::time::{Duration, MissedTickBehavior, interval};

    // Initial decision based on the on-disk snapshot.  We
    // intentionally do *not* gate this on `link_check_enabled`:
    // a user who turned the feature off probably wants the
    // pre-existing list scrubbed too, but we also don't want
    // to re-download on every restart for a feature they
    // disabled.  Compromise: only the "stale" path triggers an
    // initial refresh, and we still respect the toggle inside
    // the refresh function below.
    let stale = match link_check::status(&cache) {
        Ok(s) => match s.last_refreshed_at {
            None => true, // never refreshed
            Some(ts) => {
                let age = chrono::Utc::now().signed_duration_since(ts).num_hours();
                age >= 24 || s.total_urls == 0
            }
        },
        Err(_) => true,
    };
    if stale {
        let enabled = settings.read().await.link_check_enabled;
        if enabled && let Err(e) = refresh_urlhaus_inner(&cache).await {
            tracing::warn!("URLhaus initial refresh failed: {e}");
        }
    }

    let mut tick = interval(Duration::from_secs(60 * 60));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Burn the immediate first tick so we don't stack on top
    // of the startup refresh above.
    tick.tick().await;

    loop {
        tick.tick().await;
        let enabled = settings.read().await.link_check_enabled;
        if !enabled {
            continue;
        }
        if let Err(e) = refresh_urlhaus_inner(&cache).await {
            tracing::warn!("URLhaus refresh failed (will retry next tick): {e}");
        }
    }
}

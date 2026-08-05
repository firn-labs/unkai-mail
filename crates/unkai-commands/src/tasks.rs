//! Nextcloud Tasks (CalDAV `VTODO`).
//!
//! Mirrors `ui/src/lib/api/tasks.ts`.

use unkai_caldav::build_vtodo_ics as caldav_build_vtodo_ics;
use unkai_caldav::create_task as caldav_create_task;
use unkai_caldav::delete_task as caldav_delete_task;
use unkai_caldav::list_task_lists as caldav_list_task_lists;
use unkai_caldav::sync_tasks as caldav_sync_tasks;
use unkai_caldav::update_task as caldav_update_task;
use unkai_core::UnkaiError;
use unkai_core::models::Task;
use unkai_core::models::TaskList;
use unkai_store::Cache;
use unkai_store::credentials;

use crate::support::{SyncStatus, load_nextcloud_account};

// ── Nextcloud Tasks (#92) ────────────────────────────────────────
//
// VTODO via CalDAV.  Nextcloud Tasks stores tasks inside the same
// CalDAV collections the Calendar app uses for VEVENTs, distinguished
// only by `supported-calendar-component-set` advertising VTODO.  We
// mirror the notes / calendar command shape:
//
//   - `list_nextcloud_task_lists`  → cache read, paints instantly.
//   - `sync_nextcloud_task_lists`  → PROPFIND, replaces the cached set.
//   - `sync_nextcloud_tasks`       → per-list sync-collection delta.
//   - `create / update / delete_nextcloud_task` → write-through
//     server-first / cache-on-success.
//   - `create_nextcloud_task_from_mail` → builds a `Task` from a
//     mail row's `(account, folder, uid, subject, from)` and writes
//     it.  Uses the `mail://account/folder/uid` URL scheme already
//     understood by `NotesView` so the TasksView "Source mail" chip
//     and a Notes mail-ref are interchangeable.

/// Cache-first list of task lists for one NC account.  Returns
/// whatever's on disk; the UI kicks off a background
/// `sync_nextcloud_task_lists` to refresh discovery and per-list
/// `sync_nextcloud_tasks` to refresh contents.
pub fn list_nextcloud_task_lists(
    nc_id: String,
    cache: &Cache,
) -> Result<Vec<TaskList>, UnkaiError> {
    Ok(cache
        .list_task_lists(&nc_id)?
        .into_iter()
        .map(|c| c.list)
        .collect())
}

/// Re-run PROPFIND for the account's task-supporting calendar
/// collections and replace the cached `task_lists` rows for that
/// account.  Mirrors `sync_nextcloud_calendars` for the VEVENT path.
pub async fn sync_nextcloud_task_lists(
    nc_id: String,
    cache: &Cache,
) -> Result<Vec<TaskList>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    // Task lists are a Nextcloud-app feature (#413) — DAV/local
    // sources never advertise the capability, so return empty
    // rather than probing a layout that doesn't exist.
    if !account.is_nextcloud() {
        return Ok(Vec::new());
    }
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let lists = caldav_list_task_lists(
        &nc_id,
        &account.server_url,
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await?;
    cache.apply_task_lists_delta(&nc_id, &lists)?;
    // Return the cache-read view, not the raw discovery — the user's
    // local-only `hidden` / `muted` flags are stored in the cache
    // row and would be wiped from the frontend state if we returned
    // `lists` (which carries the defaults `false` / `false` from the
    // PROPFIND).  `apply_task_lists_delta` deliberately doesn't touch
    // those columns on upsert, so the cache row still has the user's
    // persisted values after this call.
    Ok(cache
        .list_task_lists(&nc_id)?
        .into_iter()
        .map(|c| c.list)
        .collect())
}

/// Cache-first list of every task across the account's lists.
pub fn list_nextcloud_tasks(nc_id: String, cache: &Cache) -> Result<Vec<Task>, UnkaiError> {
    cache.list_tasks_for_account(&nc_id).map_err(Into::into)
}

/// Incrementally sync one task list via RFC 6578 sync-collection.
/// The frontend calls this per-list on view focus and on a 120 s
/// background timer, mirroring `sync_nextcloud_calendars`.
pub async fn sync_nextcloud_tasks(
    nc_id: String,
    list_id: String,
    cache: &Cache,
) -> Result<Vec<Task>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    // See sync_nextcloud_task_lists — no Tasks app outside Nextcloud.
    if !account.is_nextcloud() {
        return cache.list_tasks_for_account(&nc_id).map_err(Into::into);
    }
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let cached = cache
        .get_task_list(&list_id)?
        .ok_or_else(|| UnkaiError::Other(format!("no cached task list with id '{list_id}'")))?;
    let prev = cached.sync_token.as_deref();
    let delta = caldav_sync_tasks(
        &list_id,
        &account.server_url,
        &cached.list.path,
        &account.username,
        &app_password,
        prev,
        &account.trusted_certs,
    )
    .await?;
    let upserts: Vec<Task> = delta.upserts.iter().flat_map(|r| r.tasks.clone()).collect();
    cache.apply_tasks_delta(
        &list_id,
        &upserts,
        &delta.deleted_hrefs,
        delta.new_sync_token.as_deref(),
    )?;
    cache.list_tasks_for_account(&nc_id).map_err(Into::into)
}

/// Create a new task in `list_id`.  Generates a fresh UUID for the
/// VTODO UID so two clients can't collide on the wire, builds the
/// VTODO body, PUTs with `If-None-Match: *`, and on success persists
/// the row to the local cache.
#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
pub async fn create_nextcloud_task(
    nc_id: String,
    list_id: String,
    summary: String,
    description: Option<String>,
    due_unix: Option<i64>,
    due_tz: Option<String>,
    priority: Option<u8>,
    url: Option<String>,
    cache: &Cache,
) -> Result<Task, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let cached_list = cache
        .get_task_list(&list_id)?
        .ok_or_else(|| UnkaiError::Other(format!("no cached task list with id '{list_id}'")))?;
    let uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let due = due_unix.and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0));
    let task = Task {
        uid: uid.clone(),
        task_list_id: list_id.clone(),
        href: String::new(),
        etag: String::new(),
        summary,
        description,
        status: "NEEDS-ACTION".to_string(),
        priority: priority.unwrap_or(0),
        due,
        completed: None,
        created: Some(now),
        last_modified: Some(now),
        url,
        categories: Vec::new(),
        ics_raw: String::new(),
    };
    let ics = caldav_build_vtodo_ics(&task, due_tz.as_deref());
    let outcome = caldav_create_task(
        &account.server_url,
        &cached_list.list.path,
        &account.username,
        &app_password,
        &uid,
        &ics,
        &account.trusted_certs,
    )
    .await?;
    let stored = Task {
        href: outcome.href,
        etag: outcome.etag,
        ics_raw: ics,
        ..task
    };
    cache.upsert_task(&list_id, &stored)?;
    Ok(stored)
}

/// Apply a partial update to a task.  Each field is optional; the
/// caller sends only what changed.  Toggling completion flips both
/// `status` and `completed` in lockstep so a CalDAV client reading
/// only one column still sees the right answer.
#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
pub async fn update_nextcloud_task(
    nc_id: String,
    list_id: String,
    uid: String,
    etag: String,
    summary: Option<String>,
    description: Option<String>,
    status: Option<String>,
    priority: Option<u8>,
    due_unix: Option<i64>,
    due_tz: Option<String>,
    clear_due: Option<bool>,
    completed_unix: Option<i64>,
    clear_completed: Option<bool>,
    url: Option<String>,
    categories: Option<Vec<String>>,
    cache: &Cache,
) -> Result<Task, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let mut task = cache
        .get_task(&list_id, &uid)?
        .ok_or_else(|| UnkaiError::Other(format!("no cached task '{uid}' in list '{list_id}'")))?;

    if let Some(v) = summary {
        task.summary = v;
    }
    if let Some(v) = description {
        task.description = if v.is_empty() { None } else { Some(v) };
    }
    if let Some(v) = status {
        task.status = v;
        // Keep COMPLETED timestamp in lockstep with STATUS so a
        // CalDAV client that only reads one column still gets the
        // right answer (RFC 5545 §3.8.1.11).
        if task.status.eq_ignore_ascii_case("COMPLETED") && task.completed.is_none() {
            task.completed = Some(chrono::Utc::now());
        } else if !task.status.eq_ignore_ascii_case("COMPLETED") {
            task.completed = None;
        }
    }
    if let Some(v) = priority {
        task.priority = v;
    }
    if clear_due.unwrap_or(false) {
        task.due = None;
    } else if let Some(ts) = due_unix {
        task.due = chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0);
    }
    if clear_completed.unwrap_or(false) {
        task.completed = None;
    } else if let Some(ts) = completed_unix {
        task.completed = chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0);
    }
    if let Some(v) = url {
        task.url = if v.is_empty() { None } else { Some(v) };
    }
    if let Some(v) = categories {
        task.categories = v;
    }
    task.last_modified = Some(chrono::Utc::now());

    let ics = caldav_build_vtodo_ics(&task, due_tz.as_deref());
    let outcome = caldav_update_task(
        &task.href,
        &account.username,
        &app_password,
        &etag,
        &ics,
        &account.trusted_certs,
    )
    .await?;
    task.etag = outcome.etag;
    task.ics_raw = ics;
    cache.upsert_task(&list_id, &task)?;
    Ok(task)
}

/// Delete a task.  Server first (4xx surfaces before we touch local
/// state); cache delete only runs on success.
pub async fn delete_nextcloud_task(
    nc_id: String,
    list_id: String,
    uid: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let task = cache
        .get_task(&list_id, &uid)?
        .ok_or_else(|| UnkaiError::Other(format!("no cached task '{uid}' in list '{list_id}'")))?;
    caldav_delete_task(
        &task.href,
        &account.username,
        &app_password,
        &task.etag,
        &account.trusted_certs,
    )
    .await?;
    cache.delete_task(&list_id, &uid)?;
    Ok(())
}

/// Build a fresh task from an open mail message and write it to
/// `list_id`.  The task's `summary` defaults to the mail subject,
/// `description` includes the sender, and `url` is the in-app
/// `mail://account/folder/uid` reference — `NotesView` already
/// understands that scheme, so the TasksView "Source mail" chip
/// and a Notes mail-ref click route through the same handler.
#[allow(clippy::too_many_arguments)] // Tauri command: each arg maps to a frontend invoke parameter
pub async fn create_nextcloud_task_from_mail(
    nc_id: String,
    list_id: String,
    mail_account_id: String,
    folder: String,
    uid: u32,
    subject: String,
    from: String,
    cache: &Cache,
) -> Result<Task, UnkaiError> {
    // URL-encode the folder path so a folder like `INBOX/Work`
    // survives the round-trip through a `mail://` URL.  We keep
    // the encoding minimal — replace `%` and `/` literally and
    // leave everything else for the URL crate's reverse path on
    // the frontend (which is the same one NotesView uses).
    let encoded_folder = folder
        .chars()
        .map(|c| match c {
            '/' => "/".to_string(),
            ' ' => "%20".to_string(),
            '%' => "%25".to_string(),
            _ => c.to_string(),
        })
        .collect::<String>();
    let source_url = format!("mail://{mail_account_id}/{encoded_folder}/{uid}");
    let summary = if subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        subject
    };
    let description = if from.trim().is_empty() {
        None
    } else {
        Some(format!("From: {from}"))
    };

    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let cached_list = cache
        .get_task_list(&list_id)?
        .ok_or_else(|| UnkaiError::Other(format!("no cached task list with id '{list_id}'")))?;
    let task_uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let task = Task {
        uid: task_uid.clone(),
        task_list_id: list_id.clone(),
        href: String::new(),
        etag: String::new(),
        summary,
        description,
        status: "NEEDS-ACTION".to_string(),
        priority: 0,
        due: None,
        completed: None,
        created: Some(now),
        last_modified: Some(now),
        url: Some(source_url),
        categories: Vec::new(),
        ics_raw: String::new(),
    };
    // No DUE on a from-mail task — the TZID parameter is a no-op
    // here, so we pass None and the builder's UTC-Z fallback path
    // is irrelevant.  When the user adds a reminder later, the
    // editor's save sends the user's IANA zone and the rebuild
    // picks up the TZID-anchored form.
    let ics = caldav_build_vtodo_ics(&task, None);
    let outcome = caldav_create_task(
        &account.server_url,
        &cached_list.list.path,
        &account.username,
        &app_password,
        &task_uid,
        &ics,
        &account.trusted_certs,
    )
    .await?;
    let stored = Task {
        href: outcome.href,
        etag: outcome.etag,
        ics_raw: ics,
        ..task
    };
    cache.upsert_task(&list_id, &stored)?;
    Ok(stored)
}

/// Flip a Nextcloud Tasks task list's sidebar visibility (#92).
/// Mirrors `set_nextcloud_calendar_hidden` — purely client-side,
/// no CalDAV traffic.  `hidden = true` removes the list from the
/// TasksView sidebar AND drops its tasks from the All / Today /
/// Overdue / Completed virtual buckets so the user can declutter
/// without unsubscribing from the underlying collection.
pub fn set_nextcloud_task_list_hidden(
    task_list_id: String,
    hidden: bool,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    cache.set_task_list_hidden(&task_list_id, hidden)?;
    Ok(())
}

/// Layer-2 mute toggle for a task list (#92).  Mirrors
/// `set_nextcloud_calendar_muted` — keeps the list in the
/// sidebar but suppresses its tasks from the virtual buckets so
/// the user can dim a list without dropping it from the sidebar
/// entirely.  Controlled by clicking the row's colour swatch.
pub fn set_nextcloud_task_list_muted(
    task_list_id: String,
    muted: bool,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    cache.set_task_list_muted(&task_list_id, muted)?;
    Ok(())
}

/// Aggregate sync status for one NC account's task lists —
/// powers the "Task lists" SyncStatusRow in NextcloudSettings.
/// Mirrors `get_calendars_sync_status` / `get_contacts_sync_status`.
pub fn get_tasks_sync_status(nc_id: String, cache: &Cache) -> Result<SyncStatus, UnkaiError> {
    let (count, last_synced_at) = cache.tasks_sync_summary(&nc_id)?;
    let last_synced_iso = last_synced_at
        .and_then(|secs| chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0))
        .map(|dt| dt.to_rfc3339());
    Ok(SyncStatus {
        count: count.max(0) as u32,
        last_synced_at: last_synced_iso,
    })
}

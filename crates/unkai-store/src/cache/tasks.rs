//! Nextcloud Tasks cache (#92).
//!
//! Mirrors the calendar cache shape — `task_lists` holds the
//! collection metadata (sync token, ctag, colour, read-only flag),
//! `tasks` holds one row per VTODO with the parsed fields the UI
//! needs.  Reads paint instantly from the cache; writes go to the
//! server first and then update the cache on success, same pattern
//! the notes and calendar layers use.

use chrono::{TimeZone, Utc};
use rusqlite::{OptionalExtension, params};

use unkai_core::models::{Task, TaskList};

use crate::cache::{Cache, CacheError};

/// Per-task-list sync bookmark.  `sync_token` is the RFC 6578
/// opaque cursor; `last_synced_at` powers the "Last synced N
/// minutes ago" hint in the header.
#[derive(Debug, Clone, Default)]
pub struct TaskListSyncState {
    pub sync_token: Option<String>,
    pub last_synced_at: Option<i64>,
}

/// Cache shape of a task-list row.  Wraps `TaskList` with the
/// sync bookmark so the UI can render "Last synced …" without
/// joining tables.
#[derive(Debug, Clone)]
pub struct CachedTaskList {
    pub list: TaskList,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub last_synced_at: Option<i64>,
}

impl Cache {
    /// Apply a fresh PROPFIND result for one NC account, replacing the
    /// cached task-list rows with the server's current set.  The
    /// `sync_token` and `last_synced_at` columns are preserved across
    /// the upsert — discovery doesn't change those, only `sync_tasks`
    /// does.
    pub fn apply_task_lists_delta(
        &self,
        nc_id: &str,
        lists: &[TaskList],
    ) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        let live_ids: std::collections::HashSet<String> =
            lists.iter().map(|l| l.id.clone()).collect();
        let cached_ids: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT id FROM task_lists WHERE nextcloud_account_id = ?1")?;
            stmt.query_map(params![nc_id], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect()
        };
        for id in cached_ids {
            if !live_ids.contains(&id) {
                tx.execute("DELETE FROM tasks WHERE task_list_id = ?1", params![id])?;
                tx.execute("DELETE FROM task_lists WHERE id = ?1", params![id])?;
            }
        }

        for l in lists {
            tx.execute(
                "INSERT INTO task_lists
                    (id, nextcloud_account_id, path, name, display_name, color, read_only)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (id) DO UPDATE SET
                    path         = excluded.path,
                    name         = excluded.name,
                    display_name = excluded.display_name,
                    color        = excluded.color,
                    read_only    = excluded.read_only",
                params![
                    l.id,
                    l.nextcloud_account_id,
                    l.path,
                    l.name,
                    l.display_name,
                    l.color,
                    l.read_only as i64,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// List every cached task list for one NC account, ordered
    /// alphabetically by display name.  Returns the sync metadata
    /// alongside the canonical `TaskList` so the UI can render a
    /// "Last synced" hint without a second query.
    pub fn list_task_lists(&self, nc_id: &str) -> Result<Vec<CachedTaskList>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, nextcloud_account_id, path, name, display_name, color, read_only, hidden,
                    sync_token, ctag, last_synced_at
             FROM task_lists
             WHERE nextcloud_account_id = ?1
             ORDER BY display_name COLLATE NOCASE ASC, name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map(params![nc_id], |r| {
            Ok(CachedTaskList {
                list: TaskList {
                    id: r.get(0)?,
                    nextcloud_account_id: r.get(1)?,
                    path: r.get(2)?,
                    name: r.get(3)?,
                    display_name: r.get(4)?,
                    color: r.get::<_, Option<String>>(5)?,
                    read_only: r.get::<_, i64>(6)? != 0,
                    hidden: r.get::<_, i64>(7)? != 0,
                },
                sync_token: r.get::<_, Option<String>>(8)?,
                ctag: r.get::<_, Option<String>>(9)?,
                last_synced_at: r.get::<_, Option<i64>>(10)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Look up a single task list by its composite id.  Returns the
    /// path + sync bookmark needed to drive a sync round-trip.
    pub fn get_task_list(&self, list_id: &str) -> Result<Option<CachedTaskList>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT id, nextcloud_account_id, path, name, display_name, color, read_only, hidden,
                        sync_token, ctag, last_synced_at
                 FROM task_lists
                 WHERE id = ?1",
                params![list_id],
                |r| {
                    Ok(CachedTaskList {
                        list: TaskList {
                            id: r.get(0)?,
                            nextcloud_account_id: r.get(1)?,
                            path: r.get(2)?,
                            name: r.get(3)?,
                            display_name: r.get(4)?,
                            color: r.get::<_, Option<String>>(5)?,
                            read_only: r.get::<_, i64>(6)? != 0,
                            hidden: r.get::<_, i64>(7)? != 0,
                        },
                        sync_token: r.get::<_, Option<String>>(8)?,
                        ctag: r.get::<_, Option<String>>(9)?,
                        last_synced_at: r.get::<_, Option<i64>>(10)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Flip a task list's local visibility flag.  Purely client-side —
    /// never touches the CalDAV server.  Mirrors `set_calendar_hidden`.
    pub fn set_task_list_hidden(&self, task_list_id: &str, hidden: bool) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE task_lists SET hidden = ?2 WHERE id = ?1",
            params![task_list_id, hidden as i64],
        )?;
        Ok(())
    }

    /// Apply one task-list sync round.  Upserts the changed tasks,
    /// drops the deleted hrefs, and bumps the per-list sync bookmark.
    pub fn apply_tasks_delta(
        &self,
        list_id: &str,
        upserts: &[Task],
        deleted_hrefs: &[String],
        new_sync_token: Option<&str>,
    ) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let now = Utc::now().timestamp();

        for href in deleted_hrefs {
            tx.execute(
                "DELETE FROM tasks WHERE task_list_id = ?1 AND href = ?2",
                params![list_id, href],
            )?;
        }

        if !upserts.is_empty() {
            let mut stmt = tx.prepare(
                "INSERT INTO tasks
                    (task_list_id, uid, href, etag, summary, description, status, priority,
                     due_utc, completed_utc, created_utc, last_modified_utc,
                     url, categories_json, ics_raw, cached_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT (task_list_id, uid) DO UPDATE SET
                    href              = excluded.href,
                    etag              = excluded.etag,
                    summary           = excluded.summary,
                    description       = excluded.description,
                    status            = excluded.status,
                    priority          = excluded.priority,
                    due_utc           = excluded.due_utc,
                    completed_utc     = excluded.completed_utc,
                    created_utc       = excluded.created_utc,
                    last_modified_utc = excluded.last_modified_utc,
                    url               = excluded.url,
                    categories_json   = excluded.categories_json,
                    ics_raw           = excluded.ics_raw,
                    cached_at         = excluded.cached_at",
            )?;
            for t in upserts {
                let categories_json =
                    serde_json::to_string(&t.categories).unwrap_or_else(|_| "[]".to_string());
                stmt.execute(params![
                    list_id,
                    t.uid,
                    t.href,
                    t.etag,
                    t.summary,
                    t.description,
                    t.status,
                    t.priority as i64,
                    t.due.map(|d| d.timestamp()),
                    t.completed.map(|d| d.timestamp()),
                    t.created.map(|d| d.timestamp()),
                    t.last_modified.map(|d| d.timestamp()),
                    t.url,
                    categories_json,
                    t.ics_raw,
                    now,
                ])?;
            }
        }

        tx.execute(
            "UPDATE task_lists SET sync_token = ?2, last_synced_at = ?3 WHERE id = ?1",
            params![list_id, new_sync_token, now],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Single-row upsert used by `create_task` / `update_task` after
    /// the server has acknowledged the change.  Doesn't touch the
    /// sync bookmark — only `apply_tasks_delta` does.
    pub fn upsert_task(&self, list_id: &str, task: &Task) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let now = Utc::now().timestamp();
        let categories_json =
            serde_json::to_string(&task.categories).unwrap_or_else(|_| "[]".to_string());
        conn.execute(
            "INSERT INTO tasks
                (task_list_id, uid, href, etag, summary, description, status, priority,
                 due_utc, completed_utc, created_utc, last_modified_utc,
                 url, categories_json, ics_raw, cached_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT (task_list_id, uid) DO UPDATE SET
                href              = excluded.href,
                etag              = excluded.etag,
                summary           = excluded.summary,
                description       = excluded.description,
                status            = excluded.status,
                priority          = excluded.priority,
                due_utc           = excluded.due_utc,
                completed_utc     = excluded.completed_utc,
                created_utc       = excluded.created_utc,
                last_modified_utc = excluded.last_modified_utc,
                url               = excluded.url,
                categories_json   = excluded.categories_json,
                ics_raw           = excluded.ics_raw,
                cached_at         = excluded.cached_at",
            params![
                list_id,
                task.uid,
                task.href,
                task.etag,
                task.summary,
                task.description,
                task.status,
                task.priority as i64,
                task.due.map(|d| d.timestamp()),
                task.completed.map(|d| d.timestamp()),
                task.created.map(|d| d.timestamp()),
                task.last_modified.map(|d| d.timestamp()),
                task.url,
                categories_json,
                task.ics_raw,
                now,
            ],
        )?;
        Ok(())
    }

    /// Forget one task locally.  Used by `delete_task` after the
    /// server has accepted the DELETE.
    pub fn delete_task(&self, list_id: &str, uid: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM tasks WHERE task_list_id = ?1 AND uid = ?2",
            params![list_id, uid],
        )?;
        Ok(())
    }

    /// List every cached task across one NC account's task lists.
    /// The UI filters by list / completion / search client-side so
    /// the SQL stays simple.
    pub fn list_tasks_for_account(&self, nc_id: &str) -> Result<Vec<Task>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT t.task_list_id, t.uid, t.href, t.etag, t.summary, t.description,
                    t.status, t.priority, t.due_utc, t.completed_utc, t.created_utc,
                    t.last_modified_utc, t.url, t.categories_json, t.ics_raw
             FROM tasks t
             INNER JOIN task_lists tl ON tl.id = t.task_list_id
             WHERE tl.nextcloud_account_id = ?1
             ORDER BY COALESCE(t.due_utc, 8640000000000) ASC,
                      COALESCE(t.last_modified_utc, 0) DESC",
        )?;
        let rows = stmt.query_map(params![nc_id], |r| {
            let categories_json: String = r.get(13)?;
            let categories: Vec<String> =
                serde_json::from_str(&categories_json).unwrap_or_default();
            Ok(Task {
                uid: r.get(1)?,
                task_list_id: r.get(0)?,
                href: r.get(2)?,
                etag: r.get(3)?,
                summary: r.get(4)?,
                description: r.get(5)?,
                status: r.get(6)?,
                priority: r.get::<_, i64>(7)? as u8,
                due: r
                    .get::<_, Option<i64>>(8)?
                    .and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                completed: r
                    .get::<_, Option<i64>>(9)?
                    .and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                created: r
                    .get::<_, Option<i64>>(10)?
                    .and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                last_modified: r
                    .get::<_, Option<i64>>(11)?
                    .and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                url: r.get::<_, Option<String>>(12)?,
                categories,
                ics_raw: r.get(14)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Pull one task for editing.  The UI uses this to refresh a
    /// stale etag right before saving so a concurrent edit surfaces
    /// as a 412 rather than after-the-fact data loss.
    pub fn get_task(&self, list_id: &str, uid: &str) -> Result<Option<Task>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT task_list_id, uid, href, etag, summary, description,
                        status, priority, due_utc, completed_utc, created_utc,
                        last_modified_utc, url, categories_json, ics_raw
                 FROM tasks
                 WHERE task_list_id = ?1 AND uid = ?2",
                params![list_id, uid],
                |r| {
                    let categories_json: String = r.get(13)?;
                    let categories: Vec<String> =
                        serde_json::from_str(&categories_json).unwrap_or_default();
                    Ok(Task {
                        task_list_id: r.get(0)?,
                        uid: r.get(1)?,
                        href: r.get(2)?,
                        etag: r.get(3)?,
                        summary: r.get(4)?,
                        description: r.get(5)?,
                        status: r.get(6)?,
                        priority: r.get::<_, i64>(7)? as u8,
                        due: r
                            .get::<_, Option<i64>>(8)?
                            .and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                        completed: r
                            .get::<_, Option<i64>>(9)?
                            .and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                        created: r
                            .get::<_, Option<i64>>(10)?
                            .and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                        last_modified: r
                            .get::<_, Option<i64>>(11)?
                            .and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                        url: r.get::<_, Option<String>>(12)?,
                        categories,
                        ics_raw: r.get(14)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Most recent sync bookmark for a task list.
    pub fn task_list_sync_state(&self, list_id: &str) -> Result<TaskListSyncState, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT sync_token, last_synced_at FROM task_lists WHERE id = ?1",
                params![list_id],
                |r| {
                    Ok(TaskListSyncState {
                        sync_token: r.get::<_, Option<String>>(0)?,
                        last_synced_at: r.get::<_, Option<i64>>(1)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default();
        Ok(row)
    }

    /// Wipe all task lists and their tasks for an NC account — used
    /// when the user removes the Nextcloud account so we don't leak
    /// stale data.
    pub fn wipe_task_lists_for_account(&self, nc_id: &str) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM tasks WHERE task_list_id IN
                (SELECT id FROM task_lists WHERE nextcloud_account_id = ?1)",
            params![nc_id],
        )?;
        tx.execute(
            "DELETE FROM task_lists WHERE nextcloud_account_id = ?1",
            params![nc_id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(nc: &str, id: &str, name: &str) -> TaskList {
        TaskList {
            id: id.into(),
            nextcloud_account_id: nc.into(),
            path: format!("https://x/dav/cal/{name}/"),
            name: name.into(),
            display_name: name.into(),
            color: None,
            read_only: false,
            hidden: false,
        }
    }

    fn task(list_id: &str, uid: &str, summary: &str, completed: bool) -> Task {
        Task {
            uid: uid.into(),
            task_list_id: list_id.into(),
            href: format!("https://x/dav/cal/list/{uid}.ics"),
            etag: format!("etag-{uid}"),
            summary: summary.into(),
            description: None,
            status: if completed {
                "COMPLETED".into()
            } else {
                "NEEDS-ACTION".into()
            },
            priority: 0,
            due: None,
            completed: if completed {
                Some(Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap())
            } else {
                None
            },
            created: None,
            last_modified: None,
            url: None,
            categories: Vec::new(),
            ics_raw: String::new(),
        }
    }

    #[test]
    fn task_lists_delta_replaces_set() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .apply_task_lists_delta(
                "nc1",
                &[
                    list("nc1", "nc1::a", "alpha"),
                    list("nc1", "nc1::b", "beta"),
                ],
            )
            .unwrap();
        assert_eq!(cache.list_task_lists("nc1").unwrap().len(), 2);

        cache
            .apply_task_lists_delta("nc1", &[list("nc1", "nc1::b", "beta")])
            .unwrap();
        let lists = cache.list_task_lists("nc1").unwrap();
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].list.id, "nc1::b");
    }

    #[test]
    fn tasks_delta_upserts_and_deletes() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .apply_task_lists_delta("nc1", &[list("nc1", "nc1::a", "alpha")])
            .unwrap();
        cache
            .apply_tasks_delta(
                "nc1::a",
                &[
                    task("nc1::a", "t1", "Buy milk", false),
                    task("nc1::a", "t2", "Ship release", true),
                ],
                &[],
                Some("token-1"),
            )
            .unwrap();
        let tasks = cache.list_tasks_for_account("nc1").unwrap();
        assert_eq!(tasks.len(), 2);
        let st = cache.task_list_sync_state("nc1::a").unwrap();
        assert_eq!(st.sync_token.as_deref(), Some("token-1"));

        cache
            .apply_tasks_delta(
                "nc1::a",
                &[task("nc1::a", "t3", "New thing", false)],
                &[format!("https://x/dav/cal/list/{}.ics", "t1")],
                Some("token-2"),
            )
            .unwrap();
        let tasks = cache.list_tasks_for_account("nc1").unwrap();
        let uids: Vec<&str> = tasks.iter().map(|t| t.uid.as_str()).collect();
        assert!(uids.contains(&"t2"));
        assert!(uids.contains(&"t3"));
        assert!(!uids.contains(&"t1"));
    }

    #[test]
    fn wipe_account_clears_lists_and_tasks() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .apply_task_lists_delta("nc1", &[list("nc1", "nc1::a", "alpha")])
            .unwrap();
        cache
            .apply_tasks_delta("nc1::a", &[task("nc1::a", "t1", "x", false)], &[], None)
            .unwrap();

        cache.wipe_task_lists_for_account("nc1").unwrap();
        assert_eq!(cache.list_task_lists("nc1").unwrap().len(), 0);
        assert_eq!(cache.list_tasks_for_account("nc1").unwrap().len(), 0);
    }
}

<script lang="ts">
  /**
   * TasksView — rail-routed Nextcloud Tasks browser + editor (#92).
   *
   * Three-column layout mirroring NotesView:
   *
   *   Sidebar → virtual buckets (All / Today / Overdue / Completed)
   *             plus one row per task list (CalDAV collection that
   *             supports VTODO).  Counts live on the rows.
   *   List    → tasks filtered by the sidebar selection AND the
   *             search query, sorted by due (asc) then modified
   *             (desc).  Each row: checkbox + summary + due chip
   *             + optional priority + optional source-mail chip.
   *   Editor  → summary + description + due + priority +
   *             completion + source-mail link.  Inline auto-save
   *             with a "Saving / Saved / Save failed" status.
   *
   * # Backend model
   *
   * Tasks are VTODO components stored in CalDAV collections.  Reads
   * are served from the local cache (`list_nextcloud_tasks`,
   * `list_nextcloud_task_lists`) so the view paints instantly;
   * writes go to the server first (so a 412 etag mismatch surfaces
   * before we touch local state) and then upsert the cache.
   *
   * # mail:// source links
   *
   * When the user creates a task from a mail (Compose header
   * button), the task's `URL` property is set to
   * `mail://<account>/<folder>/<uid>`.  Clicking the "Source mail"
   * chip on a task row routes through the same `onopenmail`
   * handler NotesView uses, so the two integration views share one
   * mail-ref handler.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { onDestroy, onMount } from 'svelte'
  import { formatError } from './errors'
  import DateField from './DateField.svelte'
  import Icon from './Icon.svelte'
  import SearchInput from './SearchInput.svelte'
  import TimeField from './TimeField.svelte'
  import { resizableSidebar } from './resizableSidebar'

  interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
  }

  /** Mirrors `unkai_core::models::TaskList`. */
  interface TaskList {
    id: string
    nextcloud_account_id: string
    path: string
    name: string
    display_name: string
    color?: string | null
    read_only: boolean
    /** Local-only flag set via NextcloudSettings → Task lists
     *  visibility.  `true` drops the row from the sidebar and
     *  filters its tasks out of the All / Today / Overdue /
     *  Completed virtuals — same shape as the per-calendar
     *  `hidden` toggle. */
    hidden?: boolean
  }

  /** Mirrors `unkai_core::models::Task`. */
  interface Task {
    uid: string
    task_list_id: string
    href: string
    etag: string
    summary: string
    description?: string | null
    status: string
    priority: number
    /** RFC 3339 / ISO 8601 (serde DateTime<Utc>). */
    due?: string | null
    completed?: string | null
    created?: string | null
    last_modified?: string | null
    url?: string | null
    categories: string[]
    ics_raw: string
  }

  type Selection =
    | { kind: 'all' }
    | { kind: 'today' }
    | { kind: 'overdue' }
    | { kind: 'completed' }
    | { kind: 'list'; listId: string }

  interface Props {
    /** `mail://acc/folder/uid` link handler — the same handler
     *  NotesView uses for inline mail references.  Clicking a
     *  task's source-mail chip routes through this. */
    onopenmail?: (accountId: string, folder: string, uid: number) => void
  }
  const { onopenmail }: Props = $props()

  let accounts = $state<NextcloudAccount[]>([])
  let accountId = $state('')
  let lists = $state<TaskList[]>([])
  let tasks = $state<Task[]>([])
  let loading = $state(false)
  let error = $state('')
  let selection = $state<Selection>({ kind: 'all' })

  let selectedUid = $state<string | null>(null)
  let draftSummary = $state('')
  let draftDescription = $state('')
  /** Local-zone date half of the due editor (`YYYY-MM-DD`).  Paired
   *  with `draftDueTime` to drive the shared DateField + TimeField
   *  components — same split EventEditor uses for VEVENT start /
   *  end so both views speak the same picker UX. */
  let draftDueDate = $state('')
  /** Local-zone time half of the due editor (`HH:MM`).  Empty when
   *  the task has no due date OR when the user wants an all-day
   *  due (we still write the time as `00:00` in that case — VTODO
   *  doesn't model "due without a time of day" separately). */
  let draftDueTime = $state('')
  /** Set true by `openTask` right before it overwrites the due
   *  fields, so the auto-save `$effect` watching them can tell
   *  "loaded from server" apart from "user picked a new date".
   *  Reset back to false on the next effect run.  Without this,
   *  every click on a task row would round-trip a PUT through
   *  the server that didn't actually change anything. */
  let skipNextDueSave = $state(false)
  let draftPriority = $state(0)
  let draftEtag = $state('')
  let draftListId = $state('')
  let saveStatus = $state<'' | 'saving' | 'saved' | 'error'>('')

  /** Search query for the list pane — independent of sidebar
   *  selection so the user can search "within Personal" or
   *  "across all tasks" without switching. */
  let searchQuery = $state('')

  const REFRESH_INTERVAL_MS = 120_000
  let pollTimer: number | null = null
  let saveTimer: ReturnType<typeof setTimeout> | null = null

  onMount(async () => {
    await loadAccounts()
  })

  onDestroy(() => {
    if (pollTimer !== null) window.clearInterval(pollTimer)
    if (saveTimer !== null) clearTimeout(saveTimer)
  })

  async function loadAccounts() {
    try {
      const list = await invoke<NextcloudAccount[]>('get_nextcloud_accounts')
      accounts = list
      if (list.length >= 1 && !accountId) {
        accountId = list[0].id
        await loadFromCache()
        startPolling()
        void syncNow()
      }
    } catch (e) {
      error = formatError(e) || 'Failed to load Nextcloud accounts'
    }
  }

  async function selectAccount(id: string) {
    accountId = id
    lists = []
    tasks = []
    selectedUid = null
    selection = { kind: 'all' }
    searchQuery = ''
    await loadFromCache()
    startPolling()
    void syncNow()
  }

  function startPolling() {
    if (pollTimer !== null) window.clearInterval(pollTimer)
    pollTimer = window.setInterval(() => {
      void syncNow()
    }, REFRESH_INTERVAL_MS)
  }

  async function loadFromCache() {
    if (!accountId) return
    loading = true
    error = ''
    try {
      const [tl, ts] = await Promise.all([
        invoke<TaskList[]>('list_nextcloud_task_lists', { ncId: accountId }),
        invoke<Task[]>('list_nextcloud_tasks', { ncId: accountId }),
      ])
      lists = tl
      tasks = ts
      // If the selected task no longer exists post-cache reload,
      // drop the editor pane back to the empty state.
      if (selectedUid != null && !ts.some((t) => t.uid === selectedUid)) {
        selectedUid = null
      }
    } catch (e) {
      error = formatError(e) || 'Failed to load tasks'
    } finally {
      loading = false
    }
  }

  /** Discovery + per-list sync, in series.  Network refresh — the
   *  cache read above is what paints the view; this just keeps
   *  things up-to-date in the background. */
  async function syncNow() {
    if (!accountId) return
    try {
      const tl = await invoke<TaskList[]>('sync_nextcloud_task_lists', {
        ncId: accountId,
      })
      lists = tl
      // Sync each list's contents in series so a slow server
      // doesn't fan out N parallel requests.
      let latestTasks: Task[] = tasks
      for (const list of tl) {
        try {
          latestTasks = await invoke<Task[]>('sync_nextcloud_tasks', {
            ncId: accountId,
            listId: list.id,
          })
        } catch (e) {
          // One list failing shouldn't kill the whole sync.
          console.warn(`sync_nextcloud_tasks failed for ${list.id}`, e)
        }
      }
      tasks = latestTasks
    } catch (e) {
      if (tasks.length === 0) {
        error = formatError(e) || 'Failed to sync tasks'
      } else {
        console.warn('background tasks sync failed', e)
      }
    }
  }

  // ── Selection-driven filtering ───────────────────────────────
  const startOfTodayUtcMs = $derived.by(() => {
    const d = new Date()
    d.setHours(0, 0, 0, 0)
    return d.getTime()
  })
  const endOfTodayUtcMs = $derived(startOfTodayUtcMs + 24 * 60 * 60 * 1000)

  function dueMs(t: Task): number | null {
    return t.due ? new Date(t.due).getTime() : null
  }

  /** Set of task-list ids the user has marked visible
   *  (`hidden !== true`).  Drives the virtual-bucket filters and
   *  the sidebar render — mirroring the calendar `hidden` flag's
   *  effect on CalendarView.  When the user picks a specific
   *  list (`selection.kind === 'list'`) we honour the click even
   *  if the list is hidden, so an explicit drill-in still works
   *  while the virtuals stay decluttered. */
  const visibleListIds = $derived(
    new Set(lists.filter((l) => !l.hidden).map((l) => l.id)),
  )

  /** Tasks scoped to visible lists only — the input to every
   *  virtual bucket count and to the unfiltered "All open"
   *  filter.  Pulled into its own derived so the per-bucket
   *  expressions below stay readable. */
  const visibleTasks = $derived(
    tasks.filter((t) => visibleListIds.has(t.task_list_id)),
  )

  const filteredTasks = $derived.by((): Task[] => {
    let list: Task[]
    switch (selection.kind) {
      case 'today':
        list = visibleTasks.filter((t) => {
          if (isCompleted(t)) return false
          const d = dueMs(t)
          return d != null && d >= startOfTodayUtcMs && d < endOfTodayUtcMs
        })
        break
      case 'overdue':
        list = visibleTasks.filter((t) => {
          if (isCompleted(t)) return false
          const d = dueMs(t)
          return d != null && d < startOfTodayUtcMs
        })
        break
      case 'completed':
        list = visibleTasks.filter((t) => isCompleted(t))
        break
      case 'list': {
        // Capture the listId locally so TypeScript narrows
        // `selection` inside the filter closure (the discriminated-
        // union narrowing on `selection.kind` doesn't survive a
        // function boundary).  Honours an explicit click into a
        // hidden list — visibility filters the virtuals + sidebar,
        // not a direct drill-in.
        const listId = selection.listId
        list = tasks.filter((t) => t.task_list_id === listId)
        break
      }
      case 'all':
      default:
        list = visibleTasks.filter((t) => !isCompleted(t))
    }
    const q = searchQuery.trim().toLowerCase()
    if (q) {
      list = list.filter(
        (t) =>
          t.summary.toLowerCase().includes(q)
          || (t.description ?? '').toLowerCase().includes(q),
      )
    }
    return list.sort(compareTasks)
  })

  function compareTasks(a: Task, b: Task): number {
    // Open before completed, then by due (asc — nulls last),
    // then by last-modified (desc) as a tiebreaker.
    const aDone = isCompleted(a) ? 1 : 0
    const bDone = isCompleted(b) ? 1 : 0
    if (aDone !== bDone) return aDone - bDone
    const ad = dueMs(a)
    const bd = dueMs(b)
    if (ad == null && bd == null) {
      const am = a.last_modified ? new Date(a.last_modified).getTime() : 0
      const bm = b.last_modified ? new Date(b.last_modified).getTime() : 0
      return bm - am
    }
    if (ad == null) return 1
    if (bd == null) return -1
    return ad - bd
  }

  function isCompleted(t: Task): boolean {
    return t.status.toUpperCase() === 'COMPLETED'
  }

  // ── Sidebar counts ──────────────────────────────────────────
  // Counts are scoped to visible lists so the virtual badges match
  // what the user actually sees when they click in (a hidden list's
  // tasks aren't surfaced via the All / Today / Overdue / Completed
  // buckets — they only appear when the user explicitly drills into
  // that list from elsewhere, e.g. the visibility checkbox UI in
  // Settings).
  const allCount = $derived(visibleTasks.filter((t) => !isCompleted(t)).length)
  const todayCount = $derived(
    visibleTasks.filter((t) => {
      if (isCompleted(t)) return false
      const d = dueMs(t)
      return d != null && d >= startOfTodayUtcMs && d < endOfTodayUtcMs
    }).length,
  )
  const overdueCount = $derived(
    visibleTasks.filter((t) => {
      if (isCompleted(t)) return false
      const d = dueMs(t)
      return d != null && d < startOfTodayUtcMs
    }).length,
  )
  const completedCount = $derived(visibleTasks.filter((t) => isCompleted(t)).length)
  function listOpenCount(id: string): number {
    return tasks.filter((t) => t.task_list_id === id && !isCompleted(t)).length
  }

  function selectionMatches(s: Selection, candidate: Selection): boolean {
    if (s.kind !== candidate.kind) return false
    if (s.kind === 'list' && candidate.kind === 'list') return s.listId === candidate.listId
    return true
  }

  // ── Editor open/save ────────────────────────────────────────
  function openTask(t: Task) {
    selectedUid = t.uid
    draftListId = t.task_list_id
    draftSummary = t.summary
    draftDescription = t.description ?? ''
    const split = utcIsoToLocalSplit(t.due)
    skipNextDueSave = true
    draftDueDate = split.date
    draftDueTime = split.time
    draftPriority = t.priority
    draftEtag = t.etag
    saveStatus = ''
    if (saveTimer !== null) {
      clearTimeout(saveTimer)
      saveTimer = null
    }
  }

  function clearSelection() {
    selectedUid = null
    saveStatus = ''
    if (saveTimer !== null) {
      clearTimeout(saveTimer)
      saveTimer = null
    }
  }

  /** Convert a UTC RFC-3339 instant to the split DateField /
   *  TimeField values the editor binds (`YYYY-MM-DD` + `HH:MM`),
   *  both in the user's local zone.  Returns empty strings when
   *  `iso` is null so the picker reads "Pick a date" rather than
   *  whatever today is.  Same idiom EventEditor uses to feed the
   *  same component pair. */
  function utcIsoToLocalSplit(
    iso: string | null | undefined,
  ): { date: string; time: string } {
    if (!iso) return { date: '', time: '' }
    const d = new Date(iso)
    if (Number.isNaN(d.getTime())) return { date: '', time: '' }
    const pad = (n: number) => String(n).padStart(2, '0')
    return {
      date: `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`,
      time: `${pad(d.getHours())}:${pad(d.getMinutes())}`,
    }
  }

  /** Combine the DateField + TimeField split values into a UTC
   *  epoch in seconds.  `null` when no date is set; an empty time
   *  defaults to midnight local (we don't model "all-day due"
   *  separately — VTODO's DUE is always a single instant).  Browsers
   *  parse `YYYY-MM-DDTHH:MM` (no offset) as local time, so the
   *  Date constructor stores it as the correct UTC instant. */
  function localSplitToUnixSecs(date: string, time: string): number | null {
    if (!date) return null
    const ms = new Date(`${date}T${time || '00:00'}`).getTime()
    if (Number.isNaN(ms)) return null
    return Math.round(ms / 1000)
  }

  /** Visible (non-hidden) task lists.  Drives the new-task list
   *  picker so a hidden list isn't offered as a create target —
   *  matches the sidebar's filter. */
  const visibleLists = $derived(lists.filter((l) => !l.hidden))
  let firstNewListId = $derived(visibleLists[0]?.id ?? '')

  /** Inline auto-save with 800 ms debounce — same shape as the
   *  Notes editor.  The `saveStatus` flickers Saving → Saved →
   *  empty so the user sees a confirmation without a toast. */
  function scheduleSave() {
    if (selectedUid == null || !accountId) return
    saveStatus = 'saving'
    if (saveTimer !== null) clearTimeout(saveTimer)
    saveTimer = setTimeout(saveNow, 800)
  }

  /** `YYYY-MM-DD` for today in the user's local zone — same
   *  shape DateField's `value` accepts. */
  function todayLocalDate(): string {
    const d = new Date()
    const pad = (n: number) => String(n).padStart(2, '0')
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
  }

  // Auto-save when DateField / TimeField mutate the bound values.
  // The shared pickers are bind-only (no onchange callback), so we
  // hook reactivity here.  Reading both pieces at the top of the
  // effect — before any guard — so Svelte tracks them as deps
  // unconditionally (per CLAUDE-style guidance, conditional reads
  // inside an effect can silently lose reactivity).
  //
  // `openTask` flips `skipNextDueSave` true right before it
  // overwrites the drafts so "loaded a different task" doesn't
  // spawn a no-op PUT.
  //
  // When the user picks a time without a date, auto-fill the date
  // to today so the reminder writes back as "today at <time>"
  // rather than silently dropping (a time-only value has no
  // instant to save).  The date mutation re-triggers this effect
  // — we set `skipNextDueSave` first so the re-fire is a no-op,
  // and call `scheduleSave` directly so the save still happens
  // for the combined value the user just produced.
  $effect(() => {
    const date = draftDueDate
    const time = draftDueTime
    if (skipNextDueSave) {
      skipNextDueSave = false
      return
    }
    if (!date && time) {
      skipNextDueSave = true
      draftDueDate = todayLocalDate()
      scheduleSave()
      return
    }
    scheduleSave()
  })

  async function saveNow() {
    if (selectedUid == null || !accountId) return
    const uid = selectedUid
    const listId = draftListId
    const dueUnix = localSplitToUnixSecs(draftDueDate, draftDueTime)
    try {
      const updated = await invoke<Task>('update_nextcloud_task', {
        ncId: accountId,
        listId,
        uid,
        etag: draftEtag,
        summary: draftSummary,
        description: draftDescription,
        priority: draftPriority,
        dueUnix,
        clearDue: dueUnix === null,
      })
      tasks = tasks.map((t) => (t.uid === updated.uid ? updated : t))
      if (uid === selectedUid) draftEtag = updated.etag
      saveStatus = 'saved'
      setTimeout(() => {
        if (saveStatus === 'saved') saveStatus = ''
      }, 1500)
    } catch (e) {
      console.warn('save task failed', e)
      saveStatus = 'error'
    }
  }

  async function toggleCompletion(t: Task) {
    if (!accountId) return
    const becomeCompleted = !isCompleted(t)
    try {
      const updated = await invoke<Task>('update_nextcloud_task', {
        ncId: accountId,
        listId: t.task_list_id,
        uid: t.uid,
        etag: t.etag,
        status: becomeCompleted ? 'COMPLETED' : 'NEEDS-ACTION',
      })
      tasks = tasks.map((x) => (x.uid === updated.uid ? updated : x))
      if (selectedUid === updated.uid) draftEtag = updated.etag
    } catch (e) {
      error = formatError(e) || 'Failed to update task'
    }
  }

  async function deleteSelected() {
    if (!accountId || selectedUid == null) return
    const t = tasks.find((x) => x.uid === selectedUid)
    if (!t) return
    if (!confirm(`Delete "${t.summary || '(untitled task)'}"? This cannot be undone.`)) return
    try {
      await invoke('delete_nextcloud_task', {
        ncId: accountId,
        listId: t.task_list_id,
        uid: t.uid,
      })
      tasks = tasks.filter((x) => x.uid !== t.uid)
      selectedUid = null
      saveStatus = ''
    } catch (e) {
      error = formatError(e) || 'Failed to delete task'
    }
  }

  async function quickDelete(t: Task) {
    if (!accountId) return
    try {
      await invoke('delete_nextcloud_task', {
        ncId: accountId,
        listId: t.task_list_id,
        uid: t.uid,
      })
      tasks = tasks.filter((x) => x.uid !== t.uid)
      if (selectedUid === t.uid) {
        selectedUid = null
        saveStatus = ''
      }
    } catch (e) {
      error = formatError(e) || 'Failed to delete task'
    }
  }

  // ── New task ────────────────────────────────────────────────
  let creating = $state(false)
  let newTaskListId = $state('')

  function startCreate() {
    // Default the new task into whichever list the sidebar is
    // focused on (when the user is on a real list).  Otherwise
    // land in the first list — same heuristic NotesView uses.
    const seed =
      selection.kind === 'list'
        ? selection.listId
        : firstNewListId
    newTaskListId = seed || ''
    if (!newTaskListId) {
      error = 'Connect a Nextcloud account with at least one task list first.'
      return
    }
    creating = true
  }

  async function commitCreate(summary: string) {
    creating = false
    if (!accountId || !newTaskListId || !summary.trim()) return
    try {
      const created = await invoke<Task>('create_nextcloud_task', {
        ncId: accountId,
        listId: newTaskListId,
        summary: summary.trim(),
      })
      tasks = [created, ...tasks]
      openTask(created)
      selection = { kind: 'list', listId: newTaskListId }
    } catch (e) {
      error = formatError(e) || 'Failed to create task'
    }
  }

  // ── Source-mail link parsing ────────────────────────────────
  /** A `mail://account/folder/uid` URL the Tasks editor renders
   *  as a "Source mail" chip.  Returns `null` for any other URL,
   *  including http(s) — those are still shown but as a plain
   *  click-through link. */
  function parseSourceMail(
    url: string | null | undefined,
  ): { accountId: string; folder: string; uid: number } | null {
    if (!url || !url.startsWith('mail://')) return null
    const rest = url.slice('mail://'.length)
    // Split on the *first* `/` to get the account id, then the
    // *last* `/` of what remains to peel off the trailing uid.
    // Folder names can contain `/` (`INBOX/Work`) so this is the
    // shape the NotesView mail-ref handler expects.
    const firstSlash = rest.indexOf('/')
    if (firstSlash < 0) return null
    const acct = rest.slice(0, firstSlash)
    const remainder = rest.slice(firstSlash + 1)
    const lastSlash = remainder.lastIndexOf('/')
    if (lastSlash < 0) return null
    const folderEncoded = remainder.slice(0, lastSlash)
    const uidStr = remainder.slice(lastSlash + 1)
    const uid = Number.parseInt(uidStr, 10)
    if (!Number.isFinite(uid) || uid <= 0) return null
    let folder: string
    try {
      folder = decodeURIComponent(folderEncoded)
    } catch {
      folder = folderEncoded
    }
    return { accountId: acct, folder, uid }
  }

  function openSourceMail(t: Task) {
    const ref = parseSourceMail(t.url)
    if (ref && onopenmail) onopenmail(ref.accountId, ref.folder, ref.uid)
  }

  // ── Display helpers ─────────────────────────────────────────
  function fmtDue(iso: string | null | undefined): string {
    if (!iso) return ''
    const d = new Date(iso)
    if (Number.isNaN(d.getTime())) return ''
    const today = new Date()
    today.setHours(0, 0, 0, 0)
    const due = new Date(d)
    due.setHours(0, 0, 0, 0)
    const dayMs = 24 * 60 * 60 * 1000
    const diffDays = Math.round((due.getTime() - today.getTime()) / dayMs)
    if (diffDays === 0) return 'Today'
    if (diffDays === 1) return 'Tomorrow'
    if (diffDays === -1) return 'Yesterday'
    return d.toLocaleDateString([], { month: 'short', day: 'numeric' })
  }

  function dueClasses(t: Task): string {
    if (isCompleted(t)) return 'text-surface-400'
    const ms = dueMs(t)
    if (ms == null) return 'text-surface-500'
    if (ms < startOfTodayUtcMs) return 'text-red-500'
    if (ms < endOfTodayUtcMs) return 'text-warning-500'
    return 'text-surface-500'
  }

  function priorityLabel(p: number): string {
    if (p >= 1 && p <= 4) return 'High'
    if (p === 5) return 'Medium'
    if (p >= 6 && p <= 9) return 'Low'
    return ''
  }

  function priorityClasses(p: number): string {
    if (p >= 1 && p <= 4) return 'bg-red-500/15 text-red-500'
    if (p === 5) return 'bg-warning-500/15 text-warning-500'
    if (p >= 6 && p <= 9) return 'bg-surface-200 dark:bg-surface-700 text-surface-600 dark:text-surface-300'
    return ''
  }

  function listColor(id: string): string {
    const list = lists.find((l) => l.id === id)
    return list?.color || '#6b7280'
  }

  function listName(id: string): string {
    return lists.find((l) => l.id === id)?.display_name ?? ''
  }

  // Inline-create input element + value
  let newSummary = $state('')
  let newSummaryInput: HTMLInputElement | undefined = $state()
  $effect(() => {
    if (creating && newSummaryInput) {
      newSummaryInput.focus()
    }
  })
</script>

<div class="h-full flex bg-surface-50 dark:bg-surface-900">
  {#if accounts.length === 0 && !loading}
    <div class="flex-1 flex items-center justify-center text-sm text-surface-500 p-8 text-center">
      Connect a Nextcloud account first (Settings → Nextcloud) to use Tasks.
    </div>
  {:else}
    <!-- Sidebar: virtuals + task lists.  Mirrors NotesView's nav
         column shape so the two integration views feel coherent. -->
    <aside
      class="shrink-0 border-r border-surface-200 dark:border-surface-700 bg-surface-100 dark:bg-surface-800 flex flex-col text-sm"
      use:resizableSidebar={{ key: 'tasks.navSidebar', defaultWidth: 224, min: 160, max: 480 }}
    >
      <!-- Primary action — same shape + filled-primary preset as
           NotesView's "+ New note" / mail Compose CTA. -->
      <div class="p-3">
        <button
          class="btn preset-filled-primary-500 w-full inline-flex items-center justify-center gap-1.5"
          onclick={startCreate}
          disabled={!accountId || lists.length === 0}
          title="New task"
          aria-label="New task"
        >
          <Icon name="plus" size={16} />
          <span>New task</span>
        </button>
      </div>

      <div class="flex-1 min-h-0 overflow-y-auto pb-2">
        <button
          class="tasks-side-row {selectionMatches(selection, { kind: 'all' }) ? 'is-active' : ''}"
          onclick={() => (selection = { kind: 'all' })}
        >
          <span class="tasks-side-icon"><Icon name="tasks" size={16} /></span>
          <span class="flex-1 truncate text-left">All open</span>
          <span class="tasks-side-count">{allCount}</span>
        </button>
        <button
          class="tasks-side-row {selectionMatches(selection, { kind: 'today' }) ? 'is-active' : ''}"
          onclick={() => (selection = { kind: 'today' })}
        >
          <span class="tasks-side-icon text-warning-500"><Icon name="today" size={16} /></span>
          <span class="flex-1 truncate text-left">Today</span>
          <span class="tasks-side-count">{todayCount}</span>
        </button>
        <button
          class="tasks-side-row {selectionMatches(selection, { kind: 'overdue' }) ? 'is-active' : ''}"
          onclick={() => (selection = { kind: 'overdue' })}
        >
          <span class="tasks-side-icon text-red-500"><Icon name="warning" size={16} /></span>
          <span class="flex-1 truncate text-left">Overdue</span>
          <span class="tasks-side-count">{overdueCount}</span>
        </button>
        <button
          class="tasks-side-row {selectionMatches(selection, { kind: 'completed' }) ? 'is-active' : ''}"
          onclick={() => (selection = { kind: 'completed' })}
        >
          <span class="tasks-side-icon text-success-500"><Icon name="success" size={16} /></span>
          <span class="flex-1 truncate text-left">Completed</span>
          <span class="tasks-side-count">{completedCount}</span>
        </button>

        <div class="my-2 border-t border-surface-200 dark:border-surface-700"></div>

        <!-- Skip lists the user has flagged hidden in
             NextcloudSettings → Task lists.  The cache row keeps
             its `hidden` column so a list reappears the moment
             the user re-enables it — no re-sync needed. -->
        {#each lists.filter((l) => !l.hidden) as l (l.id)}
          <button
            class="tasks-side-row group {selectionMatches(selection, { kind: 'list', listId: l.id }) ? 'is-active' : ''}"
            onclick={() => (selection = { kind: 'list', listId: l.id })}
          >
            <span
              class="tasks-side-swatch"
              style:background-color={l.color || '#6b7280'}
              aria-hidden="true"
            ></span>
            <span class="flex-1 truncate text-left">{l.display_name || l.name}</span>
            <span class="tasks-side-count">{listOpenCount(l.id)}</span>
          </button>
        {/each}
        {#if lists.length === 0 && !loading}
          <p class="px-4 py-3 text-xs text-surface-500">
            No task lists yet. Create one in the Nextcloud Tasks app, then refresh here.
          </p>
        {/if}
      </div>
    </aside>

    <!-- List pane: header (search + new) → optional account picker
         → task rows. -->
    <div
      class="shrink-0 border-r border-surface-200 dark:border-surface-700 flex flex-col"
      use:resizableSidebar={{ key: 'tasks.listColumn', defaultWidth: 320, min: 240, max: 600 }}
    >
      <div class="border-b border-surface-200 dark:border-surface-700 p-2">
        <SearchInput bind:value={searchQuery} placeholder="Search tasks" />
      </div>

      {#if accounts.length > 1}
        <div class="px-3 py-1.5 border-b border-surface-200 dark:border-surface-700">
          <select
            class="select text-xs py-1 px-2 rounded-md w-full"
            value={accountId}
            onchange={(e) => selectAccount((e.currentTarget as HTMLSelectElement).value)}
          >
            {#each accounts as a (a.id)}
              <option value={a.id}>{a.display_name || a.username}</option>
            {/each}
          </select>
        </div>
      {/if}

      {#if creating}
        <!-- Inline-create row — same shape as NotesView's
             "+ Add folder" draft.  Enter commits, Escape cancels. -->
        <div class="px-3 py-2 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
          {#if visibleLists.length > 1}
            <select
              class="select text-xs py-1 px-2 rounded-md"
              bind:value={newTaskListId}
              title="Task list"
            >
              <!-- Only offer visible lists as create targets — a
                   user who's hidden a list from the sidebar isn't
                   expecting it to show up in the picker either. -->
              {#each visibleLists as l (l.id)}
                <option value={l.id}>{l.display_name || l.name}</option>
              {/each}
            </select>
          {/if}
          <input
            bind:this={newSummaryInput}
            bind:value={newSummary}
            type="text"
            class="input flex-1 text-sm px-2 py-1 rounded-md"
            placeholder="What needs doing?"
            onkeydown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault()
                const s = newSummary
                newSummary = ''
                void commitCreate(s)
              } else if (e.key === 'Escape') {
                e.preventDefault()
                creating = false
                newSummary = ''
              }
            }}
          />
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
            title="Cancel"
            aria-label="Cancel"
            onclick={() => {
              creating = false
              newSummary = ''
            }}
          >
            <Icon name="close" size={14} />
          </button>
        </div>
      {/if}

      <div class="flex-1 min-h-0 overflow-y-auto">
        {#if loading && tasks.length === 0}
          <div class="p-6 text-center text-sm text-surface-500">Loading…</div>
        {:else if error && tasks.length === 0}
          <div class="p-4 text-sm text-red-500">{error}</div>
        {:else if tasks.length === 0}
          <div class="p-6 text-center text-sm text-surface-500">
            No tasks yet.  Click <strong>New task</strong> in the sidebar to create one.
          </div>
        {:else if filteredTasks.length === 0}
          <div class="p-6 text-center text-sm text-surface-500">
            {#if searchQuery.trim()}
              No tasks match <strong>"{searchQuery.trim()}"</strong>.
            {:else}
              No tasks in this view.
            {/if}
          </div>
        {:else}
          {#each filteredTasks as t (t.uid)}
            {@const completed = isCompleted(t)}
            {@const sourceMail = parseSourceMail(t.url)}
            <div
              class="relative group border-b border-surface-100 dark:border-surface-800"
              role="listitem"
            >
              <div class="flex items-start gap-2 px-3 py-2.5
                  {selectedUid === t.uid ? 'bg-primary-500/10' : 'hover:bg-surface-100 dark:hover:bg-surface-800'}">
                <!-- Checkbox lives outside the row's main button
                     so a click on it doesn't also open the editor.
                     This is the standard pattern for task UIs:
                     the box is a separate target from the body. -->
                <button
                  type="button"
                  class="mt-0.5 w-4 h-4 rounded border border-surface-400 dark:border-surface-500
                         inline-flex items-center justify-center shrink-0
                         {completed ? 'bg-success-500 border-success-500 text-white' : 'hover:border-primary-500'}"
                  title={completed ? 'Mark as not done' : 'Mark as done'}
                  aria-label={completed ? 'Mark as not done' : 'Mark as done'}
                  aria-pressed={completed}
                  onclick={(e) => {
                    e.stopPropagation()
                    void toggleCompletion(t)
                  }}
                >
                  {#if completed}
                    <Icon name="success" size={12} />
                  {/if}
                </button>
                <button
                  class="flex-1 text-left min-w-0"
                  onclick={() => openTask(t)}
                >
                  <div class="flex items-center gap-2 min-w-0">
                    <span
                      class="text-sm truncate flex-1
                             {completed ? 'line-through text-surface-400' : 'text-surface-900 dark:text-surface-100'}"
                    >
                      {t.summary || '(untitled task)'}
                    </span>
                    {#if t.due}
                      <span class="text-xs shrink-0 {dueClasses(t)}">{fmtDue(t.due)}</span>
                    {/if}
                  </div>
                  <div class="mt-1 flex flex-wrap items-center gap-1.5">
                    <!-- List swatch chip: the row's list color +
                         display name, so when "All open" is the
                         active view the user can tell at a glance
                         which list each task belongs to. -->
                    {#if selection.kind !== 'list'}
                      <span class="inline-flex items-center gap-1 text-[10px] text-surface-500">
                        <span
                          class="w-2 h-2 rounded-full"
                          style:background-color={listColor(t.task_list_id)}
                          aria-hidden="true"
                        ></span>
                        <span class="truncate max-w-32">{listName(t.task_list_id)}</span>
                      </span>
                    {/if}
                    {#if priorityLabel(t.priority)}
                      <span class="text-[10px] px-1.5 py-0.5 rounded {priorityClasses(t.priority)}">
                        {priorityLabel(t.priority)}
                      </span>
                    {/if}
                    {#if sourceMail}
                      <span class="text-[10px] inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-primary-500/10 text-primary-500">
                        <Icon name="email-envelope" size={11} />
                        <span>Mail</span>
                      </span>
                    {/if}
                  </div>
                </button>
              </div>

              <!-- Quick actions cluster — same shape NotesView /
                   MailList use.  Pointer-events disabled when
                   hidden so the row's primary click still hits. -->
              <div
                class="absolute right-1 top-1.5 flex items-center gap-0.5 opacity-0 pointer-events-none transition-opacity
                       group-hover:opacity-100 group-hover:pointer-events-auto
                       focus-within:opacity-100 focus-within:pointer-events-auto"
              >
                {#if sourceMail}
                  <button
                    type="button"
                    class="w-7 h-7 rounded-md flex items-center justify-center bg-surface-50/90 dark:bg-surface-800/90 hover:bg-surface-200 dark:hover:bg-surface-700 shadow-sm"
                    title="Open source mail"
                    aria-label="Open source mail"
                    onclick={(e) => {
                      e.stopPropagation()
                      openSourceMail(t)
                    }}
                  >
                    <Icon name="email-envelope" size={14} />
                  </button>
                {/if}
                <button
                  type="button"
                  class="w-7 h-7 rounded-md flex items-center justify-center bg-surface-50/90 dark:bg-surface-800/90 hover:bg-red-500/20 hover:text-red-500 shadow-sm"
                  title="Delete"
                  aria-label="Delete"
                  onclick={(e) => {
                    e.stopPropagation()
                    void quickDelete(t)
                  }}
                >
                  <Icon name="trash" size={14} />
                </button>
              </div>
            </div>
          {/each}
        {/if}
      </div>
    </div>

    <!-- Editor pane -->
    <div class="flex-1 min-w-0 flex flex-col">
      {#if selectedUid == null}
        <div class="flex-1 flex items-center justify-center text-sm text-surface-500">
          Select a task from the list, or create a new one.
        </div>
      {:else}
        {@const open = tasks.find((t) => t.uid === selectedUid)}
        {#if open}
          {@const sourceMail = parseSourceMail(open.url)}
          <div class="px-5 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
            <input
              class="input flex-1 text-base font-semibold px-3 py-2 rounded-md"
              placeholder="Task title"
              bind:value={draftSummary}
              oninput={scheduleSave}
            />
            {#if saveStatus === 'saving'}
              <span class="text-xs text-surface-400">Saving…</span>
            {:else if saveStatus === 'saved'}
              <span class="text-xs text-success-500">Saved</span>
            {:else if saveStatus === 'error'}
              <span class="text-xs text-error-500">Save failed</span>
            {/if}
            <button
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
              onclick={() => void toggleCompletion(open)}
              title={isCompleted(open) ? 'Mark as not done' : 'Mark as done'}
              aria-label={isCompleted(open) ? 'Mark as not done' : 'Mark as done'}
              aria-pressed={isCompleted(open)}
            >
              <Icon name={isCompleted(open) ? 'success' : 'unread'} size={16} />
            </button>
            <button
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
              onclick={deleteSelected}
              title="Delete task"
              aria-label="Delete task"
            >
              <Icon name="trash" size={16} />
            </button>
            <button
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
              onclick={clearSelection}
              title="Close"
              aria-label="Close"
            >
              <Icon name="close" size={16} />
            </button>
          </div>

          <div class="flex-1 min-h-0 overflow-y-auto p-5 space-y-4 text-sm">
            <div>
              <span class="block text-xs text-surface-500 mb-1">Reminder</span>
              <!-- Shared DateField + TimeField pair (#126) — same
                   calendar-grid + slot-list pickers EventEditor
                   renders for VEVENT start / end, so a user
                   switching between the Tasks and Calendar views
                   gets the same date/time UX in both places.
                   Native `<input type="datetime-local">` was
                   replaced for that consistency reason — the
                   browser-supplied picker varies wildly across
                   platforms and doesn't match the rest of the
                   form vocabulary.
                   Both pickers render unconditionally — the user
                   can pick a time first and we'll auto-fill the
                   date to today (handled by the auto-default
                   `$effect` further up), so the field reads as a
                   single "Reminder" unit rather than a sequential
                   date-then-time gate. -->
              <div class="flex items-center gap-2">
                <div class="flex-1 min-w-0 max-w-48">
                  <DateField
                    id="tasks-editor-reminder-date"
                    ariaLabel="Reminder date"
                    bind:value={draftDueDate}
                  />
                </div>
                <div class="w-28">
                  <TimeField
                    id="tasks-editor-reminder-time"
                    ariaLabel="Reminder time"
                    bind:value={draftDueTime}
                  />
                </div>
                {#if draftDueDate || draftDueTime}
                  <button
                    class="ml-1 text-xs text-surface-500 hover:text-surface-900 dark:hover:text-surface-100 underline"
                    onclick={() => {
                      // Suppress the next reactive save: the two
                      // assignments below would otherwise fire the
                      // due-watcher $effect twice (once per write).
                      skipNextDueSave = true
                      draftDueDate = ''
                      draftDueTime = ''
                      scheduleSave()
                    }}
                  >Clear reminder</button>
                {/if}
              </div>
            </div>

            <div>
              <label class="block text-xs text-surface-500 mb-1" for="tasks-editor-priority">Priority</label>
              <select
                id="tasks-editor-priority"
                class="select text-sm px-2 py-1 rounded-md"
                bind:value={draftPriority}
                onchange={scheduleSave}
              >
                <option value={0}>None</option>
                <option value={1}>High</option>
                <option value={5}>Medium</option>
                <option value={9}>Low</option>
              </select>
            </div>

            <div>
              <label class="block text-xs text-surface-500 mb-1" for="tasks-editor-desc">Description</label>
              <textarea
                id="tasks-editor-desc"
                class="textarea w-full text-sm px-2 py-1 rounded-md"
                rows="6"
                placeholder="Add details…"
                bind:value={draftDescription}
                oninput={scheduleSave}
              ></textarea>
            </div>

            {#if sourceMail}
              <div>
                <p class="block text-xs text-surface-500 mb-1">Source</p>
                <button
                  class="inline-flex items-center gap-1.5 text-sm px-2 py-1 rounded-md
                         bg-primary-500/10 text-primary-500 hover:bg-primary-500/20"
                  onclick={() => openSourceMail(open)}
                  title="Open the mail this task was created from"
                >
                  <Icon name="email-envelope" size={14} />
                  <span>Open source mail</span>
                </button>
              </div>
            {:else if open.url}
              <div>
                <p class="block text-xs text-surface-500 mb-1">URL</p>
                <a
                  class="text-sm text-primary-500 hover:underline break-all"
                  href={open.url}
                  target="_blank"
                  rel="noopener noreferrer"
                >{open.url}</a>
              </div>
            {/if}

            <div class="text-xs text-surface-500">
              <span>List:</span>
              <span
                class="inline-flex items-center gap-1 ml-1"
              >
                <span
                  class="w-2 h-2 rounded-full"
                  style:background-color={listColor(open.task_list_id)}
                  aria-hidden="true"
                ></span>
                <span>{listName(open.task_list_id)}</span>
              </span>
            </div>
          </div>
        {/if}
      {/if}
    </div>
  {/if}
</div>

<style>
  /* Sidebar rows match NotesView's `.notes-side-row` shape so the
     two integration views speak the same visual language.  Defined
     here rather than imported from NotesView to keep this view
     self-contained — same trade-off NotesView itself made. */
  :global(.tasks-side-row) {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.375rem 0.625rem;
    color: inherit;
    background: transparent;
    border: none;
    cursor: pointer;
    text-align: left;
  }
  :global(.tasks-side-row:hover) {
    background: var(--color-surface-100);
  }
  :global([data-mode='dark'] .tasks-side-row:hover) {
    background: var(--color-surface-800);
  }
  :global(.tasks-side-row.is-active) {
    background: rgba(var(--color-primary-500) r g b / 0.15);
    color: var(--color-primary-600);
    font-weight: 500;
  }
  :global([data-mode='dark'] .tasks-side-row.is-active) {
    color: var(--color-primary-300);
  }
  :global(.tasks-side-icon) {
    flex-shrink: 0;
    width: 1.125rem;
    text-align: center;
  }
  :global(.tasks-side-count) {
    flex-shrink: 0;
    font-size: 0.6875rem;
    color: var(--color-surface-500);
    padding-left: 0.25rem;
  }
  :global(.tasks-side-swatch) {
    flex-shrink: 0;
    width: 0.875rem;
    height: 0.875rem;
    border-radius: 0.25rem;
    border: 1px solid var(--color-surface-300);
  }
  :global([data-mode='dark'] .tasks-side-swatch) {
    border-color: var(--color-surface-700);
  }
</style>

<script lang="ts">
  /**
   * NextcloudSettings — manage Nextcloud server connection(s).
   *
   * Flow for connecting:
   * 1. User types their NC server URL and clicks "Connect".
   * 2. We call `start_nextcloud_login` to get a browser URL + poll handle.
   * 3. We open the URL via `open_url` (system default browser).
   * 4. We poll `poll_nextcloud_login` every 2s until the server returns
   *    the app password (user approved in the browser) or the user cancels.
   *
   * The app password is stored in the OS keychain by the backend; this
   * component only ever sees the account metadata.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { formatError } from './errors'
  import SyncStatusRow from './SyncStatusRow.svelte'
  import Toggle from './Toggle.svelte'
  import { ncProbeBundle, ncRestoreBundle } from './settingsBundle'

  // ── Props ───────────────────────────────────────────────────
  interface Props {
    /** Fired whenever the connected-account set or its capabilities
     *  change (account added, removed, or capabilities re-probed).
     *  App.svelte uses this to re-aggregate `ncCaps` so the IconRail
     *  lights up the new integration icons without waiting for the
     *  Settings panel to close. */
    onnextcloudchanged?: () => void
  }
  let { onnextcloudchanged }: Props = $props()

  // ── Types (mirror the Rust models) ──────────────────────────
  interface NextcloudCapabilities {
    version?: string | null
    talk: boolean
    files: boolean
    caldav: boolean
    carddav: boolean
    /** Nextcloud Office (Collabora, app id `richdocuments`).
     *  When `true` the attachment-click flow can open `.docx` /
     *  `.odt` / `.xlsx` etc. in an embedded editor; when `false`
     *  the UI falls back to plain download. */
    office?: boolean
    /** Nextcloud Notes app installed + enabled.  Chip-only signal. */
    notes?: boolean
    /** Nextcloud Tasks app installed + enabled.  Chip-only signal. */
    tasks?: boolean
  }
  interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
    capabilities?: NextcloudCapabilities | null
    /** User-trusted self-signed cert fingerprints (#253).
     *  Mirrors the Rust `NextcloudAccount::trusted_certs`. */
    trusted_certs?: TrustedCert[]
  }
  /** Mirror of the Rust `TrustedCert` shape — the same struct
   *  is used by IMAP/SMTP and now Nextcloud HTTPS. */
  interface TrustedCert {
    der: number[]
    sha256: string
    host: string
    added_at: number
  }
  /** Mirror of `ProbedCert` from the Rust side. */
  interface ProbedCertEntry {
    der: number[]
    sha256: string
  }
  interface ProbedCert {
    chain: ProbedCertEntry[]
    host: string
  }
  interface LoginFlowInit {
    login_url: string
    poll_token: string
    poll_endpoint: string
  }

  // Returned by sync_nextcloud_contacts so the UI can show
  // "12 new, 1 removed" instead of a bare "done".
  interface SyncContactsReport {
    nc_account_id: string
    books_synced: number
    upserted: number
    deleted: number
    errors: string[]
  }
  // Per-account sync view-model. Keyed by account id so rows
  // render independently and one account's spinner doesn't block
  // another's. `lastSyncedAt` comes from the cache via
  // `get_{contacts,calendars}_sync_status` — RFC 3339 from Rust,
  // which `SyncStatusRow` formats as a relative phrase.
  interface SyncRowState {
    syncing: boolean
    lastSyncedAt: string | null
    count: number
    error: string
  }

  // Same Tauri payload shape for both contacts and calendars
  // sync-status reads.
  interface SyncStatus {
    last_synced_at: string | null
    count: number
  }

  // ── State ───────────────────────────────────────────────────
  let accounts = $state<NextcloudAccount[]>([])
  let loading = $state(true)
  let error = $state('')

  // Per-account contacts/calendars state, keyed by NC account id.
  // Lives outside `accounts` so resorting/refreshing the list
  // doesn't wipe in-flight sync status.
  let contactsState = $state<Record<string, SyncRowState>>({})
  let calendarsState = $state<Record<string, SyncRowState>>({})
  /** Sync-row state for the new "Task lists" block (#92) — same
   *  shape as `contactsState` / `calendarsState` so the row
   *  speaks the same SyncStatusRow vocabulary. */
  let tasksState = $state<Record<string, SyncRowState>>({})

  // Per-account cached calendar list for the visibility checkboxes
  // under "Calendars". Reloaded after a sync + after a visibility
  // toggle so the UI reflects the current `hidden` column.
  interface CalendarSummary {
    id: string
    nextcloud_account_id: string
    display_name: string
    color: string | null
    last_synced_at: string | null
    hidden?: boolean
    /** CalDAV-derived read-only flag (#236) — informational
     *  for this surface; the visibility checkboxes still
     *  apply to read-only calendars (the user can choose to
     *  hide a shared calendar from the sidebar). */
    read_only?: boolean
  }
  let calendarsList = $state<Record<string, CalendarSummary[]>>({})

  // Per-account cached task-list summaries for the visibility
  // checkboxes under "Tasks" (#92).  Mirrors `CalendarSummary` /
  // `calendarsList` — the cache row carries the local `hidden`
  // flag the TasksView sidebar reads when filtering.
  interface TaskListSummary {
    id: string
    nextcloud_account_id: string
    display_name: string
    name: string
    color: string | null
    hidden?: boolean
    read_only?: boolean
  }
  let taskListsList = $state<Record<string, TaskListSummary[]>>({})

  // ── Settings backup target (#168) ──────────────────────────

  async function loadCalendarsList(ncId: string) {
    try {
      const list = await invoke<CalendarSummary[]>('get_cached_calendars', { ncId })
      calendarsList[ncId] = list
    } catch (e) {
      console.warn('get_cached_calendars failed for', ncId, e)
    }
  }

  /** Pull cached task lists for one NC account — feeds the
   *  visibility checkbox list under "Tasks".  Mirrors
   *  `loadCalendarsList`.  Failures are non-fatal: we just leave
   *  the previous list in place so a flaky cache read doesn't
   *  blank the visibility section. */
  async function loadTaskListsList(ncId: string) {
    try {
      const list = await invoke<TaskListSummary[]>('list_nextcloud_task_lists', { ncId })
      taskListsList[ncId] = list
    } catch (e) {
      console.warn('list_nextcloud_task_lists failed for', ncId, e)
    }
  }

  /** Flip a calendar's `hidden` flag. Optimistic: update the local
   *  list first so the checkbox commits instantly, then invoke the
   *  Rust command. If the command errors we roll back and surface
   *  the error through the calendars sync row. */
  async function toggleCalendarHidden(ncId: string, calendarId: string, hidden: boolean) {
    const list = calendarsList[ncId] ?? []
    const prev = list.slice()
    calendarsList[ncId] = list.map((c) =>
      c.id === calendarId ? { ...c, hidden } : c,
    )
    try {
      await invoke('set_nextcloud_calendar_hidden', { calendarId, hidden })
    } catch (e) {
      calendarsList[ncId] = prev
      const state = calendarsState[ncId]
      if (state) state.error = formatError(e) || 'Failed to toggle calendar visibility'
    }
  }

  /** Flip a task list's `hidden` flag.  Same optimistic-update
   *  pattern as `toggleCalendarHidden`.  Rollback on backend
   *  failure surfaces an error on the calendars sync row (no
   *  dedicated task-list row yet — the toggle lives in the
   *  same visual block). */
  async function toggleTaskListHidden(
    ncId: string,
    taskListId: string,
    hidden: boolean,
  ) {
    const list = taskListsList[ncId] ?? []
    const prev = list.slice()
    taskListsList[ncId] = list.map((c) =>
      c.id === taskListId ? { ...c, hidden } : c,
    )
    try {
      await invoke('set_nextcloud_task_list_hidden', { taskListId, hidden })
    } catch (e) {
      taskListsList[ncId] = prev
      const state = calendarsState[ncId]
      if (state) {
        state.error = formatError(e) || 'Failed to toggle task list visibility'
      }
    }
  }

  // Connect flow
  let serverInput = $state('')
  let connecting = $state(false)      // true while a login is in flight
  let pollTimer: number | null = null // setInterval handle, so we can cancel

  // Self-signed cert support (#253).  Mirrors AccountSetup.svelte's
  // shape: a TLS error during `start_nextcloud_login` triggers
  // `probe_server_certificate`, the user reviews fingerprints in
  // a prompt, and on confirm the chain lands in `trustedCerts` and
  // we re-run the connect.  The same trust list rides through every
  // subsequent OCS / CalDAV / CardDAV / Notes / Talk / Files call
  // because it's saved into the persisted account record.
  let trustedCerts = $state<TrustedCert[]>([])
  let pendingCert = $state<ProbedCert | null>(null)

  $effect(() => {
    loadAccounts()
    // Cleanup: cancel any in-flight polling if the component unmounts.
    return () => stopPolling()
  })

  async function loadAccounts() {
    loading = true
    error = ''
    try {
      accounts = await invoke<NextcloudAccount[]>('get_nextcloud_accounts')
      // Seed sync-row state for any new accounts and refresh the
      // cached counts + last-sync timestamps for existing ones.
      // Failures are non-fatal — we just keep the old values so a
      // transient cache hiccup doesn't blank the row.
      for (const a of accounts) {
        ensureRow(contactsState, a.id)
        ensureRow(calendarsState, a.id)
        ensureRow(tasksState, a.id)
        await refreshContactsStatus(a.id)
        await refreshCalendarsStatus(a.id)
        await refreshTasksStatus(a.id)
        await loadCalendarsList(a.id)
        await loadTaskListsList(a.id)
      }
      // Background-refresh the capability snapshot for every account
      // so newly-installed Nextcloud apps (Office, Talk, …) light up
      // their chip without needing a disconnect / reconnect. Done
      // *after* the synchronous loads so the panel paints with the
      // cached chip set immediately and updates in place once the
      // server replies. Failures are swallowed in Rust — a flaky
      // network just keeps the previous snapshot.
      void Promise.all(
        accounts.map(async (a) => {
          try {
            const fresh = await invoke<NextcloudAccount>(
              'refresh_nextcloud_capabilities',
              { ncId: a.id },
            )
            // Patch the local list in place — Svelte 5 picks up the
            // new capabilities on the next render. Match by id in
            // case the user removed an account mid-refresh.
            accounts = accounts.map((x) => (x.id === fresh.id ? fresh : x))
          } catch (e) {
            console.warn('refresh_nextcloud_capabilities failed for', a.id, e)
          }
        }),
      )
    } catch (e) {
      error = formatError(e) || 'Failed to load Nextcloud connections'
    } finally {
      loading = false
    }
  }

  function ensureRow(map: Record<string, SyncRowState>, id: string) {
    if (!map[id]) {
      map[id] = { syncing: false, lastSyncedAt: null, count: 0, error: '' }
    }
  }

  async function refreshContactsStatus(ncId: string) {
    try {
      const s = await invoke<SyncStatus>('get_contacts_sync_status', { ncId })
      contactsState[ncId].lastSyncedAt = s.last_synced_at
      contactsState[ncId].count = s.count
    } catch (e) {
      console.warn('get_contacts_sync_status failed for', ncId, e)
    }
  }

  async function refreshCalendarsStatus(ncId: string) {
    try {
      const s = await invoke<SyncStatus>('get_calendars_sync_status', { ncId })
      calendarsState[ncId].lastSyncedAt = s.last_synced_at
      calendarsState[ncId].count = s.count
    } catch (e) {
      console.warn('get_calendars_sync_status failed for', ncId, e)
    }
  }

  /** Mirror of `refreshCalendarsStatus` for the Task lists row.
   *  Backed by `get_tasks_sync_status` which aggregates count +
   *  max(last_synced_at) across the cached task_lists. */
  async function refreshTasksStatus(ncId: string) {
    try {
      const s = await invoke<SyncStatus>('get_tasks_sync_status', { ncId })
      if (tasksState[ncId]) {
        tasksState[ncId].lastSyncedAt = s.last_synced_at
        tasksState[ncId].count = s.count
      }
    } catch (e) {
      console.warn('get_tasks_sync_status failed for', ncId, e)
    }
  }

  /**
   * Trigger a fresh contacts sync for one NC account.
   *
   * The backend returns a `SyncContactsReport` with upsert/delete
   * counts so we can show something concrete in the UI. Errors
   * encountered on individual addressbooks are surfaced per-account
   * but don't block other accounts.
   */
  async function syncContacts(acct: NextcloudAccount) {
    const state = contactsState[acct.id]
    if (!state || state.syncing) return
    state.syncing = true
    state.error = ''
    try {
      const report = await invoke<SyncContactsReport>('sync_nextcloud_contacts', {
        ncId: acct.id,
      })
      if (report.errors.length > 0) {
        state.error = report.errors.join('; ')
      }
      await refreshContactsStatus(acct.id)
    } catch (e) {
      state.error = formatError(e) || 'Sync failed'
    } finally {
      state.syncing = false
    }
  }

  /** Mirror of `syncContacts` for calendars — same backend pattern,
      same UI shape, both feed the same `SyncStatusRow` component. */
  async function syncCalendars(acct: NextcloudAccount) {
    const state = calendarsState[acct.id]
    if (!state || state.syncing) return
    state.syncing = true
    state.error = ''
    try {
      const report = await invoke<{ errors: string[] }>(
        'sync_nextcloud_calendars',
        { ncId: acct.id },
      )
      if (report.errors.length > 0) {
        state.error = report.errors.join('; ')
      }
      await refreshCalendarsStatus(acct.id)
      // A sync reconciles the calendar list server-side — could add,
      // rename, or remove calendars. Refresh the per-account list so
      // the visibility checkboxes reflect what's actually there now.
      await loadCalendarsList(acct.id)
    } catch (e) {
      state.error = formatError(e) || 'Sync failed'
    } finally {
      state.syncing = false
    }
  }

  /** Mirror of `syncCalendars` for the Task lists row (#92).
   *  Drives `sync_nextcloud_task_lists` which PROPFINDs the
   *  user's calendar home and keeps only collections that
   *  advertise VTODO support, then per-list `sync_nextcloud_tasks`
   *  for each one so the cache picks up the latest tasks
   *  alongside the latest list metadata. */
  async function syncTasks(acct: NextcloudAccount) {
    const state = tasksState[acct.id]
    if (!state || state.syncing) return
    state.syncing = true
    state.error = ''
    try {
      const lists = await invoke<TaskListSummary[]>(
        'sync_nextcloud_task_lists',
        { ncId: acct.id },
      )
      // Pull the latest tasks for every discovered list so the
      // virtual-bucket badges in TasksView reflect the current
      // server contents.  Failures on one list don't block the
      // others — log and keep going so a single-list problem
      // doesn't blank the whole status row.
      for (const list of lists) {
        try {
          await invoke('sync_nextcloud_tasks', {
            ncId: acct.id,
            listId: list.id,
          })
        } catch (e) {
          console.warn('sync_nextcloud_tasks failed for', list.id, e)
        }
      }
      await refreshTasksStatus(acct.id)
      await loadTaskListsList(acct.id)
    } catch (e) {
      state.error = formatError(e) || 'Sync failed'
    } finally {
      state.syncing = false
    }
  }

  /** First-time sync after a successful connect (#318).  Runs the
   *  contacts and calendars sync in parallel for the just-added
   *  account — but only for capabilities the server actually exposes
   *  (no point hitting CalDAV on a server with `caldav: false`).
   *  After both finish, re-notify the parent so any capabilities the
   *  background `refresh_nextcloud_capabilities` probe surfaced
   *  late (or any newly-discovered calendars) feed into `ncCaps`. */
  async function autoSyncNewAccount(ncId: string) {
    const acct = accounts.find((a) => a.id === ncId)
    if (!acct) return
    const tasks: Promise<void>[] = []
    if (acct.capabilities?.carddav) {
      tasks.push(syncContacts(acct))
    }
    if (acct.capabilities?.caldav) {
      tasks.push(syncCalendars(acct))
    }
    // Task lists piggy-back on CalDAV (VTODO collections) but
    // need their own discovery + per-list sync round-trip, so
    // gate on the Tasks app capability and queue alongside the
    // others.  Errors are swallowed in `syncTasks`; we just
    // schedule the work.
    if (acct.capabilities?.tasks && acct.capabilities?.caldav) {
      tasks.push(syncTasks(acct))
    }
    if (tasks.length === 0) return
    await Promise.allSettled(tasks)
    onnextcloudchanged?.()
  }

  /** Heuristic: does `msg` look like a TLS-trust failure?  Same
   *  fingerprint-style match `AccountSetup.svelte` uses for IMAP. */
  function looksLikeCertError(msg: string): boolean {
    const m = msg.toLowerCase()
    return (
      m.includes('certificate') ||
      m.includes('cert ') ||
      m.includes('unknownissuer') ||
      m.includes('untrustedissuer') ||
      m.includes('badcertificate') ||
      m.includes('tls handshake') ||
      m.includes('invalid peer certificate')
    )
  }

  /** Pull host (and explicit port if any) out of a server URL so
   *  we can hand it to `probe_server_certificate`.  Defaults to
   *  443 — the only port Nextcloud realistically serves on. */
  function hostPortFromUrl(url: string): { host: string; port: number } | null {
    try {
      const u = new URL(url)
      return {
        host: u.hostname,
        port: u.port ? Number.parseInt(u.port, 10) : 443,
      }
    } catch {
      return null
    }
  }

  async function handleNcCertError() {
    pendingCert = null
    const url = serverInput.trim()
    const normalised = /^https?:\/\//.test(url) ? url : `https://${url}`
    const target = hostPortFromUrl(normalised)
    if (!target) {
      error = 'Could not parse the server URL.'
      return
    }
    try {
      const probed = await invoke<ProbedCert>('probe_server_certificate', {
        host: target.host,
        port: target.port,
      })
      pendingCert = probed
    } catch (e: any) {
      error = `Could not retrieve the server certificate: ${formatError(e) || 'unknown error'}`
    }
  }

  function trustPendingNcCert() {
    if (!pendingCert) return
    const addedAt = Math.floor(Date.now() / 1000)
    const host = pendingCert.host
    const additions: TrustedCert[] = pendingCert.chain.map((entry) => ({
      der: entry.der,
      sha256: entry.sha256,
      host,
      added_at: addedAt,
    }))
    trustedCerts = [...trustedCerts, ...additions]
    pendingCert = null
    // Retry the connect now that the chain is trusted.  Same idiom
    // AccountSetup uses after `trustPendingCert`.
    void startConnect()
  }

  function dismissPendingNcCert() {
    pendingCert = null
  }

  async function startConnect() {
    error = ''
    const url = serverInput.trim()
    if (!url) {
      error = 'Please enter your Nextcloud server URL.'
      return
    }
    // Normalise: tolerate "cloud.example.com" by assuming https. NC
    // never supports plain http in practice, so we don't add that path.
    const normalised = /^https?:\/\//.test(url) ? url : `https://${url}`

    connecting = true
    try {
      const init = await invoke<LoginFlowInit>('start_nextcloud_login', {
        serverUrl: normalised,
        // Pass the wizard-time trust list so a self-signed server
        // gets a clean handshake on the second attempt after the
        // user trusted the cert via the prompt.
        trustedCerts: trustedCerts.length > 0 ? trustedCerts : null,
      })
      // Fire-and-forget the browser open — if it fails the user can copy
      // the URL manually from a fallback we'll show below.
      try {
        await invoke('open_url', { url: init.login_url })
      } catch (e) {
        console.warn('open_url failed, user must open manually', e)
      }
      pendingLoginUrl = init.login_url
      beginPolling(init)
    } catch (e) {
      const msg = formatError(e) || 'Failed to start Nextcloud login'
      // TLS trust failure → kick off the probe + cert prompt.
      // Anything else falls through as a regular error toast.
      if (looksLikeCertError(msg)) {
        connecting = false
        await handleNcCertError()
        return
      }
      error = msg
      connecting = false
    }
  }

  // Shown so the user can click/copy the URL if auto-open didn't work.
  let pendingLoginUrl = $state('')

  function beginPolling(init: LoginFlowInit) {
    // Snapshot the count of NC accounts that already existed
    // *before* this login flow so the post-success "found a
    // backup?" probe (#168) can tell first-ever-connect apart
    // from "connecting another NC".  Recovery on first connect
    // is the supported path; subsequent NC connects deliberately
    // do not prompt — restoring would clobber the user's live
    // settings on a machine they're already running on.
    const wasFirstEverConnect = accounts.length === 0
    // 2-second cadence is a compromise between UI responsiveness and
    // not hammering the NC server. Login Flow v2 tokens live for ~20
    // minutes; we stop on success, cancel, or any unexpected error.
    pollTimer = window.setInterval(async () => {
      try {
        const result = await invoke<NextcloudAccount | null>('poll_nextcloud_login', {
          pollEndpoint: init.poll_endpoint,
          pollToken: init.poll_token,
          // Same trust list rides the polling call so the
          // post-success `fetch_capabilities` probe and the
          // saved account record both pick it up (#253).
          trustedCerts: trustedCerts.length > 0 ? trustedCerts : null,
        })
        if (result) {
          stopPolling()
          connecting = false
          pendingLoginUrl = ''
          serverInput = ''
          await loadAccounts()
          // Surface the new account's capabilities to the rest of
          // the shell immediately (#318) — without this the IconRail
          // wouldn't show Contacts / Calendar / Files / etc. until
          // the user closed Settings.
          onnextcloudchanged?.()
          // First-time sync (#318): a freshly-connected NC has no
          // local contacts/calendars yet, so the integration views
          // would open onto an empty list until the user remembers
          // to click "Sync". Kick both off in the background here
          // — `syncContacts` / `syncCalendars` drive the same UI
          // state the manual buttons do, so the SyncStatusRow
          // reflects progress and errors normally.
          void autoSyncNewAccount(result.id)
          // Recovery prompt — only on the very first NC connect
          // (per the agreed spec: a recovery option, not a sync
          // option).  Failures during the probe stay silent: a
          // server with no backup or an unreachable .json
          // shouldn't surface as an error toast.
          if (wasFirstEverConnect) {
            void promptRestoreFromNc(result.id)
          }
        }
      } catch (e) {
        stopPolling()
        connecting = false
        pendingLoginUrl = ''
        error = formatError(e) || 'Login failed'
      }
    }, 2000)
  }

  /** Probe the freshly-connected NC for a settings.json bundle;
   *  if found, ask the user whether to restore it.  Strictly
   *  silent on errors — we don't want to scare a user during the
   *  feel-good "I just connected my server" moment. */
  async function promptRestoreFromNc(ncId: string) {
    try {
      const exportedAt = await ncProbeBundle(ncId)
      if (!exportedAt) return
      const formatted = (() => {
        try {
          return new Date(exportedAt).toLocaleString()
        } catch {
          return exportedAt
        }
      })()
      const ok = confirm(
        `Found a Unkai settings backup on this Nextcloud (saved ${formatted}). Restore it now? Existing accounts on this machine are kept; the bundle just adds back any that were missing and updates metadata.`,
      )
      if (!ok) return
      await ncRestoreBundle(ncId)
      alert(
        'Settings restored. Reload the window (or restart Unkai) to see every preference apply.',
      )
    } catch (e) {
      console.warn('post-connect bundle probe / restore failed', e)
    }
  }

  function stopPolling() {
    if (pollTimer !== null) {
      window.clearInterval(pollTimer)
      pollTimer = null
    }
  }

  function cancelConnect() {
    // The server-side token just expires on its own — nothing to tell
    // Nextcloud. Local teardown is enough.
    stopPolling()
    connecting = false
    pendingLoginUrl = ''
  }

  async function removeAccount(acct: NextcloudAccount) {
    if (!confirm(`Disconnect Nextcloud ${acct.username}@${acct.server_url}?`)) return
    try {
      await invoke('remove_nextcloud_account', { id: acct.id })
      await loadAccounts()
    } catch (e) {
      error = formatError(e) || 'Failed to remove'
    }
  }
</script>

<div class="space-y-4">
  <div class="flex items-center justify-between">
    <div>
      <h2 class="text-lg font-semibold">Nextcloud</h2>
      <p class="text-xs text-surface-500">
        Connect a Nextcloud server to enable Talk, Files attachments, and calendar/contact sync.
      </p>
    </div>
  </div>

  {#if error}
    <div class="text-sm text-red-500 p-3 bg-red-500/10 rounded-md">{error}</div>
  {/if}

  {#if loading}
    <p class="text-surface-500 text-sm">Loading…</p>
  {:else}
    <!-- Connected accounts -->
    {#if accounts.length > 0}
      <div class="space-y-2">
        {#each accounts as acct (acct.id)}
          {@const cs = contactsState[acct.id]}
          <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-lg space-y-3">
            <div class="flex items-start justify-between">
              <div class="flex-1">
                <p class="font-semibold">{acct.display_name ?? acct.username}</p>
                <p class="text-sm text-surface-500 break-all">{acct.server_url}</p>
                {#if acct.capabilities}
                  <div class="flex flex-wrap gap-1.5 mt-2">
                    {#if acct.capabilities.version}
                      <span class="text-xs px-2 py-0.5 rounded-full bg-surface-200 dark:bg-surface-700">
                        v{acct.capabilities.version}
                      </span>
                    {/if}
                    {#if acct.capabilities.talk}
                      <span class="text-xs px-2 py-0.5 rounded-full bg-blue-500/20 text-blue-600 dark:text-blue-300">Talk</span>
                    {/if}
                    {#if acct.capabilities.files}
                      <span class="text-xs px-2 py-0.5 rounded-full bg-green-500/20 text-green-600 dark:text-green-300">Files</span>
                    {/if}
                    {#if acct.capabilities.caldav}
                      <span class="text-xs px-2 py-0.5 rounded-full bg-purple-500/20 text-purple-600 dark:text-purple-300">Calendar</span>
                    {/if}
                    {#if acct.capabilities.carddav}
                      <span class="text-xs px-2 py-0.5 rounded-full bg-orange-500/20 text-orange-600 dark:text-orange-300">Contacts</span>
                    {/if}
                    {#if acct.capabilities.office}
                      <span class="text-xs px-2 py-0.5 rounded-full bg-pink-500/20 text-pink-600 dark:text-pink-300">Office</span>
                    {/if}
                    {#if acct.capabilities.notes}
                      <span class="text-xs px-2 py-0.5 rounded-full bg-yellow-500/20 text-yellow-700 dark:text-yellow-300">Notes</span>
                    {/if}
                    {#if acct.capabilities.tasks}
                      <span class="text-xs px-2 py-0.5 rounded-full bg-cyan-500/20 text-cyan-700 dark:text-cyan-300">Tasks</span>
                    {/if}
                  </div>
                {/if}
              </div>
              <button
                class="btn btn-sm preset-outlined-error-500"
                onclick={() => removeAccount(acct)}
              >
                Disconnect
              </button>
            </div>

            <!-- Contacts sync row -->
            {#if acct.capabilities?.carddav !== false}
              {@const cls = calendarsState[acct.id]}
              <SyncStatusRow
                label="Contacts"
                count={cs?.count ?? null}
                lastSyncedAt={cs?.lastSyncedAt ?? null}
                syncing={cs?.syncing ?? false}
                error={cs?.error ?? null}
                onsync={() => syncContacts(acct)}
              />
              <!-- Calendars sync row — same component, same shape, so
                   the two surfaces stay visually identical. CalendarView
                   no longer carries its own sync UI; the user comes
                   here to refresh. -->
              {#if acct.capabilities?.caldav !== false}
                <SyncStatusRow
                  label="Calendars"
                  count={cls?.count ?? null}
                  lastSyncedAt={cls?.lastSyncedAt ?? null}
                  syncing={cls?.syncing ?? false}
                  error={cls?.error ?? null}
                  onsync={() => syncCalendars(acct)}
                />
                <!-- Per-calendar visibility. Drives the `hidden`
                     column via `set_nextcloud_calendar_hidden`, which
                     the CalendarView sidebar reads when filtering.
                     Only renders when there's something to toggle —
                     a freshly-connected account without a sync yet
                     sees just the sync row above. -->
                {#if (calendarsList[acct.id]?.length ?? 0) > 0}
                  <div class="pl-6 pb-2 pr-3">
                    <div class="text-[10px] font-semibold text-surface-500 uppercase tracking-wider mb-1">
                      Visibility
                    </div>
                    <ul class="space-y-0.5">
                      {#each calendarsList[acct.id] as c (c.id)}
                        <li>
                          <div
                            class="flex items-center gap-2 px-2 py-1 rounded hover:bg-surface-200/60 dark:hover:bg-surface-700/40 text-xs"
                          >
                            <Toggle
                              checked={!c.hidden}
                              label={c.display_name}
                              onchange={(v) =>
                                void toggleCalendarHidden(acct.id, c.id, !v)}
                            />
                            <span
                              class="w-2.5 h-2.5 rounded-sm shrink-0"
                              style="background-color: {c.color ?? '#2bb0ed'};"
                            ></span>
                            <span class="truncate" title={c.display_name}>
                              {c.display_name}
                            </span>
                          </div>
                        </li>
                      {/each}
                    </ul>
                  </div>
                {/if}
              {/if}

              <!-- Task lists block (#92) — sibling to Contacts /
                   Calendars, not nested under them.  Same
                   SyncStatusRow + Visibility list shape so the
                   three integration surfaces speak one design
                   language.  Gated on the Tasks app capability
                   (VTODO storage piggy-backs on CalDAV but the
                   Tasks app is the user-facing chip).  An account
                   with Tasks enabled but no list discovered yet
                   shows just the sync row — clicking Sync runs
                   discovery + per-list pulls in one batch. -->
              {#if acct.capabilities?.tasks}
                {@const ts = tasksState[acct.id]}
                <SyncStatusRow
                  label="Task lists"
                  count={ts?.count ?? null}
                  lastSyncedAt={ts?.lastSyncedAt ?? null}
                  syncing={ts?.syncing ?? false}
                  error={ts?.error ?? null}
                  onsync={() => syncTasks(acct)}
                />
                {#if (taskListsList[acct.id]?.length ?? 0) > 0}
                  <div class="pl-6 pb-2 pr-3">
                    <div class="text-[10px] font-semibold text-surface-500 uppercase tracking-wider mb-1">
                      Visibility
                    </div>
                    <ul class="space-y-0.5">
                      {#each taskListsList[acct.id] as l (l.id)}
                        <li>
                          <div
                            class="flex items-center gap-2 px-2 py-1 rounded hover:bg-surface-200/60 dark:hover:bg-surface-700/40 text-xs"
                          >
                            <Toggle
                              checked={!l.hidden}
                              label={l.display_name || l.name}
                              onchange={(v) =>
                                void toggleTaskListHidden(acct.id, l.id, !v)}
                            />
                            <span
                              class="w-2.5 h-2.5 rounded-sm shrink-0"
                              style="background-color: {l.color ?? '#6b7280'};"
                            ></span>
                            <span class="truncate" title={l.display_name || l.name}>
                              {l.display_name || l.name}
                            </span>
                          </div>
                        </li>
                      {/each}
                    </ul>
                  </div>
                {/if}
              {/if}
            {/if}
          </div>
        {/each}
      </div>
    {/if}

    <!-- Connect form -->
    {#if !connecting}
      <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-lg">
        <label class="text-xs text-surface-500 block mb-1" for="nc-server">
          Nextcloud server URL
        </label>
        <div class="flex gap-2">
          <input
            id="nc-server"
            class="input flex-1 px-3 py-2 text-sm rounded-md"
            placeholder="https://cloud.example.com"
            bind:value={serverInput}
            onkeydown={(e) => e.key === 'Enter' && startConnect()}
          />
          <button class="btn preset-filled-primary-500" onclick={startConnect}>
            Connect
          </button>
        </div>

        <!-- Self-signed cert trust prompt (#253).  Same shape as
             AccountSetup's IMAP-side prompt: full chain SHA-256
             fingerprints, "Trust this server" / "Cancel" CTA. -->
        {#if pendingCert}
          <div class="mt-4 p-4 rounded-md border border-warning-500/40 bg-warning-500/5">
            <p class="text-sm font-medium mb-1">
              The server's TLS certificate isn't trusted by default.
            </p>
            <p class="text-xs text-surface-500 mb-3">
              This is normal for self-hosted Nextcloud servers using a
              self-signed certificate.  Compare the fingerprint below
              with the certificate on your server before trusting.
            </p>
            <p class="text-xs mb-1">
              <span class="text-surface-500">Host:</span>
              <span class="font-mono">{pendingCert.host}</span>
            </p>
            <div class="text-xs mb-3">
              <p class="text-surface-500 mb-1">
                {#if pendingCert.chain.length === 1}
                  SHA-256 fingerprint (leaf):
                {:else}
                  SHA-256 fingerprints (leaf + {pendingCert.chain.length - 1} intermediate):
                {/if}
              </p>
              <ul class="space-y-1">
                {#each pendingCert.chain as entry, i (entry.sha256)}
                  <li class="font-mono break-all">
                    <span class="text-surface-500">{i === 0 ? 'leaf:' : `int${i}:`}</span>
                    {entry.sha256}
                  </li>
                {/each}
              </ul>
            </div>
            <div class="flex gap-2">
              <button
                type="button"
                class="btn btn-sm preset-filled-primary-500"
                onclick={trustPendingNcCert}
              >Trust this server and continue</button>
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500"
                onclick={dismissPendingNcCert}
              >Cancel</button>
            </div>
          </div>
        {/if}

        {#if trustedCerts.length > 0 && !pendingCert}
          <div class="mt-3 p-3 rounded-md border border-success-500/30 bg-success-500/5 text-xs text-surface-600 dark:text-surface-400">
            {#if trustedCerts.length === 1}
              Trusting 1 self-signed certificate for this connection.
            {:else}
              Trusting {trustedCerts.length} self-signed certificates for this connection.
            {/if}
          </div>
        {/if}
      </div>
    {:else}
      <!-- Waiting for browser auth -->
      <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-lg space-y-2">
        <p class="text-sm">
          Waiting for authorisation in your browser…
        </p>
        {#if pendingLoginUrl}
          <p class="text-xs text-surface-500">
            If nothing opened, click here:
            <a class="underline text-primary-500 break-all" href={pendingLoginUrl} target="_blank" rel="noopener">
              {pendingLoginUrl}
            </a>
          </p>
        {/if}
        <button class="btn btn-sm preset-outlined-surface-500" onclick={cancelConnect}>
          Cancel
        </button>
      </div>
    {/if}
  {/if}
</div>

<script lang="ts">
  /**
   * TalkView — sidebar-routed full-pane Nextcloud Talk room list.
   *
   * Mirrors `FilesView`'s shape (header + scrollable body + footer)
   * so the integration views feel like one app. Three actions per
   * room: open in browser, share link in email, and (at the top)
   * "+ New room" to create a fresh one.
   *
   * # Why we don't cache rooms
   *
   * Talk's `/room` endpoint is cheap (few KB) and unread counts go
   * stale the second a colleague sends a message. We refetch on a
   * 30s timer plus on focus, and on demand via the refresh button.
   * No SQLite layer — the room list is purely a UI cache.
   */

  import * as api from './api'
  import { isNextcloudSource } from './ncSources'
  import { onDestroy, onMount } from 'svelte'
  import { formatError } from './errors'
  import CreateTalkRoomModal, { type TalkRoom } from './CreateTalkRoomModal.svelte'
  import type { ComposeInitial } from './Compose.svelte'
  import Icon, { type IconName } from './Icon.svelte'
  import SearchInput from './SearchInput.svelte'
  import { m } from '../paraglide/messages'

  interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
  }

  interface Props {
    /** Open Compose with the given prefill (used for "Share link"). */
    oncompose: (initial: ComposeInitial) => void
  }
  const { oncompose }: Props = $props()

  let accounts = $state<NextcloudAccount[]>([])
  let accountId = $state('')
  let rooms = $state<TalkRoom[]>([])
  let loading = $state(false)
  let error = $state('')
  let showCreate = $state(false)
  /** Archived rooms live behind a collapsible header at the bottom
      of the list — most users rarely care about them, so we default
      to collapsed and let them expand on demand.  Persisted only
      for the lifetime of the view (no localStorage); reopening
      TalkView starts collapsed again, which matches what NC's
      web UI does. */
  let showArchived = $state(false)
  /** Free-text filter applied client-side to the room list.  Case-
   *  insensitive substring match against `display_name`.  The active
   *  / archived split is applied *after* the filter so an archived
   *  room matching the query still shows up under the Archived
   *  collapsible (and the filter result auto-expands that section). */
  let searchQuery = $state('')
  const filteredRooms = $derived.by(() => {
    const q = searchQuery.trim().toLowerCase()
    if (!q) return rooms
    return rooms.filter((r) => r.display_name.toLowerCase().includes(q))
  })
  const activeRooms = $derived(filteredRooms.filter((r) => !r.is_archived))
  const archivedRooms = $derived(filteredRooms.filter((r) => r.is_archived))

  /** Auto-expand the Archived collapsible when a search query is
   *  active and only archived rooms match — without this the user
   *  would search and see "no rooms" even though the section just
   *  needs to be opened.  Clearing the query doesn't re-collapse;
   *  that would feel jumpy. */
  $effect(() => {
    if (searchQuery.trim() && activeRooms.length === 0 && archivedRooms.length > 0) {
      showArchived = true
    }
  })

  // Periodic refresh — 30s is the same cadence the Nextcloud web UI
  // polls at. Long enough to be cheap, short enough that the unread
  // counts don't lie for more than half a minute.
  const REFRESH_INTERVAL_MS = 30_000
  let pollTimer: number | null = null

  onMount(async () => {
    await loadAccounts()
  })

  onDestroy(() => {
    if (pollTimer !== null) window.clearInterval(pollTimer)
  })

  async function loadAccounts() {
    try {
      // Nextcloud-app feature — skip generic-DAV / local sources (#413).
      const list = (
        await api.nextcloud.getNextcloudAccounts()
      ).filter(isNextcloudSource)
      accounts = list
      if (list.length === 1 && !accountId) {
        accountId = list[0].id
        await refresh()
        startPolling()
      }
    } catch (e) {
      error = formatError(e) || 'Failed to load Nextcloud accounts'
    }
  }

  async function selectAccount(id: string) {
    accountId = id
    rooms = []
    await refresh()
    startPolling()
  }

  function startPolling() {
    if (pollTimer !== null) window.clearInterval(pollTimer)
    pollTimer = window.setInterval(() => {
      // Silent refresh — don't flash the loading indicator on the
      // periodic ticks. Errors stay quiet too; the next tick retries.
      void refresh({ silent: true })
    }, REFRESH_INTERVAL_MS)
  }

  async function refresh(opts: { silent?: boolean } = {}) {
    if (!accountId) return
    if (!opts.silent) loading = true
    if (!opts.silent) error = ''
    try {
      const list = await api.talk.listTalkRooms({ ncId: accountId })
      // Sort by last activity desc within each group; the template
      // splits active vs. archived into two visual sections so we
      // don't need to interleave them here.
      list.sort((a, b) => b.last_activity - a.last_activity)
      rooms = list
    } catch (e) {
      if (!opts.silent) error = formatError(e) || 'Failed to load Talk rooms'
    } finally {
      if (!opts.silent) loading = false
    }
  }

  function openRoom(room: TalkRoom) {
    void api.system.openUrl({ url: room.web_url })
  }

  /**
   * Open Compose with a "Join the Talk room: <link>" block in the
   * body. Reuses the same `ComposeInitial.talkLink` field that the
   * MailView "Talk" action populates — the rendered HTML lives in
   * Compose's `initialBodyHtml` so the format stays consistent.
   */
  function shareRoom(room: TalkRoom) {
    oncompose({
      subject: `Join Talk: ${room.display_name}`,
      talkLink: { name: room.display_name, url: room.web_url },
    })
  }

  /** Confirm + delete a room.  Talk's API tears the room down server-
      side for everyone — there's no per-user "leave" semantic for an
      owner — so we gate behind a confirm prompt to keep accidental
      clicks from nuking a busy conversation.  Optimistically remove
      from the list on success; the next periodic refresh reconciles
      any drift. */
  let deletingToken = $state<string | null>(null)
  async function deleteRoom(room: TalkRoom) {
    if (!accountId) return
    const ok = window.confirm(
      `Delete "${room.display_name}"?\n\nThis removes the room for every participant — there's no undo.`,
    )
    if (!ok) return
    deletingToken = room.token
    try {
      await api.talk.deleteTalkRoom({ ncId: accountId, roomToken: room.token })
      rooms = rooms.filter((r) => r.token !== room.token)
    } catch (e) {
      error = formatError(e) || 'Failed to delete Talk room'
    } finally {
      deletingToken = null
    }
  }

  function onRoomCreated(room: TalkRoom) {
    // Optimistically prepend the new room — the next periodic refresh
    // (or the user's manual refresh) will reconcile any drift.
    rooms = [room, ...rooms.filter((r) => r.token !== room.token)]
  }

  function formatRelative(unix: number): string {
    if (!unix) return ''
    const now = Date.now() / 1000
    const delta = now - unix
    if (delta < 60) return 'just now'
    if (delta < 3600) return `${Math.floor(delta / 60)}m ago`
    if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`
    if (delta < 7 * 86400) return `${Math.floor(delta / 86400)}d ago`
    return new Date(unix * 1000).toLocaleDateString()
  }

  /** Stroke-icon name for each Talk room kind.  One-to-one
   *  rooms = single contact silhouette; group / public meeting
   *  rooms = the calendar glyph (Talk rooms are anchored to a
   *  scheduled meeting in the user's mental model);
   *  changelog = info. */
  function roomTypeIcon(t: TalkRoom['room_type']): IconName {
    if (t === 'one_to_one') return 'contacts'
    if (t === 'changelog') return 'info'
    return 'calendar'
  }
</script>

<div class="h-full flex flex-col bg-surface-50 dark:bg-surface-900">
  <!-- Stacked header (#522): title above its icon-only actions,
       docked LEFT so the controls stay in the viewing angle near
       the content columns on wide monitors; search centered; the
       empty right slot mirrors the left one so the search doesn't
       drift as the left cluster's width changes.  Same layout as
       SharesView / FilesView / ContactsView. -->
  <div
    class="flex items-center gap-3 px-6 py-3 border-b glass-panel"
  >
    <div class="flex-1 min-w-0 flex flex-col items-start gap-2">
      <h2 class="text-xl font-semibold truncate">Talk Rooms</h2>
      <div class="flex items-center gap-2 shrink-0">
        <!-- Primary CTA: "New room" stays filled-primary so it still
             reads as the page's main action.  Icon-only via the
             shared `plus` glyph; tooltip + aria-label carry the
             label for keyboard / screen-reader users. -->
        <button
          class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center"
          disabled={!accountId}
          onclick={() => (showCreate = true)}
          title={m.talk_view_new_room_title()}
          aria-label={m.talk_view_new_room()}
        ><Icon name="plus" size={14} /></button>
        <button
          class="btn btn-sm preset-tonal-surface inline-flex items-center justify-center"
          disabled={!accountId || loading}
          onclick={() => refresh()}
          title={loading ? m.talk_view_refreshing() : m.talk_view_refresh_title()}
          aria-label={loading ? m.talk_view_refreshing() : m.talk_view_refresh()}
        ><Icon name={loading ? 'loading' : 'refresh'} size={14} /></button>
      </div>
    </div>
    <div class="flex-1 flex justify-center min-w-0">
      <SearchInput
        bind:value={searchQuery}
        placeholder={m.talk_view_search_placeholder()}
        class="w-full max-w-md"
      />
    </div>
    <div class="flex-1"></div>
  </div>

  {#if accounts.length === 0}
    <div class="p-6 text-sm text-surface-500">
      No Nextcloud account connected. Add one under
      <strong>Settings → Nextcloud</strong> first.
    </div>
  {:else}
    {#if accounts.length > 1}
      <div class="px-5 py-2 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
        <label for="talk-account" class="text-xs text-surface-500">Account</label>
        <select
          id="talk-account"
          class="select text-sm"
          value={accountId}
          onchange={(e) => selectAccount((e.target as HTMLSelectElement).value)}
        >
          <option value="" disabled>Choose an account</option>
          {#each accounts as acc (acc.id)}
            <option value={acc.id}>{acc.display_name ?? acc.username} ({acc.server_url})</option>
          {/each}
        </select>
      </div>
    {/if}

    {#if error}
      <p class="px-5 py-2 text-sm text-error-500">{error}</p>
    {/if}

    {#if !accountId}
      <p class="p-6 text-sm text-surface-500">Pick an account to view its Talk rooms.</p>
    {:else if loading && rooms.length === 0}
      <p class="p-6 text-sm text-surface-500">Loading rooms…</p>
    {:else if rooms.length === 0}
      <div class="p-6 text-sm text-surface-500">
        No Talk rooms yet. Click <strong>+ New room</strong> to start your first conversation.
      </div>
    {:else if filteredRooms.length === 0}
      <!-- Non-empty room list but the filter narrowed everything
           to zero — distinct empty state so the user knows the
           search has hit zero rather than the account having no
           rooms at all. -->
      <p class="p-6 text-sm text-surface-500">{m.talk_view_no_matches()}</p>
    {:else}
      {#snippet roomRow(room: TalkRoom)}
        <li class="px-5 py-3 flex items-center gap-3 hover:bg-primary-500/10 {room.is_archived ? 'opacity-60' : ''}">
          <span class="flex-shrink-0 text-surface-600 dark:text-surface-300"><Icon name={roomTypeIcon(room.room_type)} size={20} /></span>

          <div class="flex-1 min-w-0">
            <div class="flex items-center gap-2">
              <span class="font-medium truncate">{room.display_name}</span>
              {#if room.unread_messages > 0}
                <span
                  class="badge text-xs flex-shrink-0
                         {room.unread_mention ? 'preset-filled-error-500' : 'preset-filled-primary-500'}"
                  title={room.unread_mention ? 'You were mentioned' : 'Unread messages'}
                >{room.unread_messages}</span>
              {/if}
            </div>
            <p class="text-xs text-surface-500 truncate">
              {formatRelative(room.last_activity)}
            </p>
          </div>

          <!-- Per-row icon-only action buttons.  Class string
               matches the canonical pattern in CLAUDE.md so every
               row's set of actions reads as siblings of one shape;
               only the destructive (delete) variant adds the
               red-on-hover overlay so it stays calm at rest. -->
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
            onclick={() => shareRoom(room)}
            title={m.talk_view_share_link_title()}
            aria-label={m.talk_view_share_link()}
          ><Icon name="share-links" size={14} /></button>
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
            onclick={() => openRoom(room)}
            title={m.talk_view_open_title()}
            aria-label={m.talk_view_open()}
          ><Icon name="open-link" size={14} /></button>
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-error-500/15 hover:text-error-500 hover:border-error-500/40"
            disabled={deletingToken === room.token}
            onclick={() => void deleteRoom(room)}
            title={deletingToken === room.token ? m.talk_view_deleting() : m.talk_view_delete_title()}
            aria-label={deletingToken === room.token ? m.talk_view_deleting() : m.talk_view_delete()}
          ><Icon name={deletingToken === room.token ? 'loading' : 'trash'} size={14} /></button>
        </li>
      {/snippet}
      <div class="flex-1 overflow-y-auto flex flex-col">
        <ul class="divide-y divide-surface-200 dark:divide-surface-800">
          {#each activeRooms as room (room.token)}
            {@render roomRow(room)}
          {/each}
        </ul>
        {#if archivedRooms.length > 0}
          <!-- Collapsible archived divider — clicking the header
               toggles a section that lists every archived room.
               Defaults collapsed; the chevron + count tells the
               user there's something hidden without forcing
               them to scroll past dimmed rows they rarely
               touch. -->
          <button
            type="button"
            class="w-full px-5 py-3 flex items-center gap-3 text-sm text-surface-500 hover:text-surface-700 dark:hover:text-surface-200 group"
            onclick={() => (showArchived = !showArchived)}
            aria-expanded={showArchived}
          >
            <span class="inline-flex items-center gap-2 shrink-0">
              <span
                class="inline-block transition-transform text-lg leading-none"
                style="transform: rotate({showArchived ? 90 : 0}deg)"
                aria-hidden="true"
              >▸</span>
              <span class="font-medium">Archived</span>
              <span class="text-xs">({archivedRooms.length})</span>
            </span>
            <span class="flex-1 h-px bg-surface-200 dark:bg-surface-700 group-hover:bg-surface-300 dark:group-hover:bg-surface-600 transition-colors"></span>
          </button>
          {#if showArchived}
            <ul class="divide-y divide-surface-200 dark:divide-surface-800">
              {#each archivedRooms as room (room.token)}
                {@render roomRow(room)}
              {/each}
            </ul>
          {/if}
        {/if}
      </div>
    {/if}
  {/if}
</div>

{#if showCreate && accountId}
  <CreateTalkRoomModal
    ncId={accountId}
    onclose={() => (showCreate = false)}
    oncreated={onRoomCreated}
  />
{/if}

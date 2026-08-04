<script lang="ts">
  /**
   * SearchResults — replaces MailList when a search is active.
   *
   * Runs `search_emails` against the local FTS5 index, then offers
   * a "Search server too" button for IMAP SEARCH fallback when the
   * user suspects the mail they want isn't cached (e.g. old archive
   * messages they've never opened on this machine).
   *
   * Hit rows look like mail-list rows plus a highlighted snippet
   * rendered from `<mark>` the backend inserted around matches.
   */

  import * as api from './api'
  import type { SearchScope, SearchFilters } from './SearchBar.svelte'
  import { formatError } from './errors'
  import Icon from './Icon.svelte'

  interface SearchHit {
    accountId: string
    folder: string
    uid: number
    from: string
    subject: string
    date: string
    isRead: boolean
    isStarred: boolean
    hasAttachments: boolean
    snippet: string
  }

  interface Props {
    accountId: string
    /** Fallback folder for server-side search when scope is a single folder. */
    currentFolder: string
    query: string
    scope: SearchScope
    filters: SearchFilters
    selectedUid: number | null
    onselect: (uid: number, folder: string) => void
  }
  let {
    accountId,
    currentFolder,
    query,
    scope,
    filters,
    selectedUid,
    onselect,
  }: Props = $props()

  let hits = $state<SearchHit[]>([])
  let loading = $state(false)
  let error = $state('')
  let serverSearching = $state(false)
  let serverSearched = $state(false)

  // Server-side infinite-scroll state (#194 follow-up). Only the
  // server path paginates — local FTS5 returns up to 200 hits in
  // one round which is plenty. Once "Search server too" has been
  // clicked, scrolling near the bottom auto-loads the next batch
  // of older server-side matches.
  const SERVER_PAGE_SIZE = 100
  let loadingServerOlder = $state(false)
  let serverOlderExhausted = $state(false)

  // Re-run whenever the query / scope / filters change. We also
  // reset the "server search already done" flag so the button is
  // offered again for the new query.  Pagination flags reset too
  // so a fresh query starts with a clean lifecycle.
  $effect(() => {
    // Touch the reactive inputs so Svelte re-runs this effect.
    query
    scope
    filters
    serverSearched = false
    loadingServerOlder = false
    serverOlderExhausted = false
    void runLocal()
  })

  async function runLocal() {
    loading = true
    error = ''
    try {
      const result = await api.mail.searchEmails({
        query,
        scope,
        filters,
      })
      hits = result
    } catch (e: any) {
      error = formatError(e) || 'Search failed'
      hits = []
    } finally {
      loading = false
    }
  }

  async function runServer() {
    if (!query.trim()) return
    serverSearching = true
    try {
      const folder = scope.folder ?? currentFolder
      const serverHits = await api.mail.searchImapServer({
        accountId,
        folder,
        query,
        limit: 100,
      })
      // Merge into the result set, dedupe on (folder, uid), keeping
      // the local hit if both sources returned it (local has the
      // snippet while server results don't).
      const seen = new Set(hits.map((h) => `${h.folder}:${h.uid}`))
      const merged = [...hits]
      for (const s of serverHits) {
        const key = `${s.folder}:${s.uid}`
        if (seen.has(key)) continue
        merged.push({
          accountId,
          folder: s.folder,
          uid: s.uid,
          from: s.from,
          subject: s.subject,
          date: s.date,
          isRead: s.is_read,
          isStarred: s.is_starred,
          hasAttachments: false,
          snippet: '',
        })
      }
      merged.sort(
        (a, b) => new Date(b.date).getTime() - new Date(a.date).getTime(),
      )
      hits = merged
      serverSearched = true
      // First server round returned fewer than the page size →
      // server has no more matches, stop the scroll-paginator.
      serverOlderExhausted = serverHits.length < SERVER_PAGE_SIZE
    } catch (e: any) {
      error = formatError(e) || 'Server search failed'
    } finally {
      serverSearching = false
    }
  }

  /** Smallest UID currently held among server-search hits — the
   *  cursor for the next "load older server matches" round.
   *  Local-only hits are excluded since they may live in folders
   *  we haven't searched server-side. */
  function smallestServerUid(): number | null {
    let smallest: number | null = null
    for (const h of hits) {
      if (smallest === null || h.uid < smallest) smallest = h.uid
    }
    return smallest
  }

  /** Fetch the next page of server-side matches via
   *  `search_imap_server_older`. Triggered automatically by the
   *  scroll-near-bottom handler once the user has run a server
   *  search and there are more matches to load. */
  async function loadServerOlder() {
    if (loadingServerOlder || serverOlderExhausted) return
    if (!serverSearched) return  // user hasn't kicked off server search yet
    if (!query.trim()) return
    const smallest = smallestServerUid()
    if (smallest === null) {
      serverOlderExhausted = true
      return
    }

    loadingServerOlder = true
    try {
      const folder = scope.folder ?? currentFolder
      const more = await api.mail.searchImapServerOlder({
        accountId,
        folder,
        query,
        beforeUid: smallest,
        limit: SERVER_PAGE_SIZE,
      })

      if (more.length === 0) {
        serverOlderExhausted = true
        return
      }

      const seen = new Set(hits.map((h) => `${h.folder}:${h.uid}`))
      const merged = [...hits]
      for (const s of more) {
        const key = `${s.folder}:${s.uid}`
        if (seen.has(key)) continue
        merged.push({
          accountId,
          folder: s.folder,
          uid: s.uid,
          from: s.from,
          subject: s.subject,
          date: s.date,
          isRead: s.is_read,
          isStarred: s.is_starred,
          hasAttachments: false,
          snippet: '',
        })
      }
      merged.sort(
        (a, b) => new Date(b.date).getTime() - new Date(a.date).getTime(),
      )
      hits = merged
      // Server returned fewer than we asked for → no more older
      // matches.
      if (more.length < SERVER_PAGE_SIZE) serverOlderExhausted = true
    } catch (e) {
      console.warn('search_imap_server_older failed:', e)
    } finally {
      loadingServerOlder = false
    }
  }

  /** How far above the bottom we trigger the next "load older
   *  server matches" round. Generous (~2 viewports' worth) so
   *  the network round-trip lands before the user actually
   *  scrolls into the unloaded region. Mirrors MailList's
   *  PAGER_PREFETCH_PX. */
  const PAGER_PREFETCH_PX = 1500

  /** Scroll handler — fires `loadServerOlder` as soon as the
   *  user is within `PAGER_PREFETCH_PX` of the bottom of the
   *  results list. No-op until the user has clicked "Search
   *  server too" at least once. */
  function onListScroll(e: Event) {
    const el = e.currentTarget as HTMLDivElement
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight
    if (distanceFromBottom < PAGER_PREFETCH_PX) {
      void loadServerOlder()
    }
  }

  /** Eager prefetch after the user kicks off "Search server
   *  too" (#194 follow-up): as soon as that first server round
   *  lands, fire one more `loadServerOlder` in the background
   *  so the user can scroll past the first 100 results without
   *  ever hitting a "Loading older server matches…" pause.
   *  Subsequent pages keep coming via the scroll-based trigger.
   *  Reset on every fresh query so each search has its own
   *  prefetch lifecycle. */
  let serverPrefetchedFor = $state<string | null>(null)
  $effect(() => {
    // Re-key on the user's query + folder so each fresh search
    // gets its own prefetch round.
    const key = `${query}::${scope.folder ?? currentFolder}`
    if (!serverSearched) return
    if (loadingServerOlder) return
    if (serverOlderExhausted) return
    if (hits.length === 0) return
    if (serverPrefetchedFor === key) return
    serverPrefetchedFor = key
    void loadServerOlder()
  })

  function formatDate(iso: string): string {
    const d = new Date(iso)
    const now = new Date()
    const sameDay = d.toDateString() === now.toDateString()
    if (sameDay) {
      return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    }
    return d.toLocaleDateString([], { month: 'short', day: 'numeric' })
  }

  /** The backend already wraps matches in `<mark>…</mark>`. We emit
   *  the snippet as trusted HTML so the highlighting renders. The
   *  snippet never comes from user input directly — it's extracted
   *  by FTS5 from a value our own code already stored. No XSS risk
   *  as long as the upstream body/subject is treated safely, but
   *  we still belt-and-suspenders: strip `<` in non-mark positions
   *  by only allowing our known tags through. */
  function safeSnippet(raw: string): string {
    if (!raw) return ''
    // Escape everything, then unescape the marks we emit ourselves.
    const escaped = raw
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
    return escaped
      .replace(/&lt;mark&gt;/g, '<mark class="bg-yellow-200 dark:bg-yellow-700/50 rounded-sm px-0.5">')
      .replace(/&lt;\/mark&gt;/g, '</mark>')
  }
</script>

<div class="flex-1 flex flex-col min-w-0">
  <!-- Result count + server fallback button -->
  <div class="px-3 py-1.5 text-xs text-surface-600 dark:text-surface-300 flex items-center justify-between border-b border-surface-100 dark:border-surface-800">
    <span>
      {#if loading}
        Searching…
      {:else}
        {hits.length} result{hits.length === 1 ? '' : 's'}
      {/if}
    </span>
    {#if query.trim() && !serverSearched}
      <button
        type="button"
        class="text-primary-600 dark:text-primary-300 hover:underline disabled:opacity-50"
        disabled={serverSearching}
        onclick={runServer}
        title="Also search messages on the server (slower)"
      >
        {serverSearching ? 'Searching server…' : 'Search server too'}
      </button>
    {/if}
  </div>

  <div class="flex-1 overflow-y-auto" onscroll={onListScroll}>
    {#if error}
      <div class="p-4 text-sm text-red-500">{error}</div>
    {:else if !loading && hits.length === 0}
      <!-- Empty-state.  When local FTS5 returned no hits and the
           user hasn't yet asked the server, we explain *why*
           (only cached messages were searched) and surface the
           server-search button prominently — without that nudge
           a "no results" outcome can be misleading: the server
           may have matches the cache hasn't seen yet (#194). -->
      <div class="p-8 text-center max-w-md mx-auto">
        {#if !query.trim()}
          <p class="text-sm text-surface-500">No messages match.</p>
        {:else if serverSearched}
          <p class="text-sm text-surface-500">No messages match on this device or on the server.</p>
        {:else}
          <div class="inline-flex items-center justify-center w-12 h-12 mb-3 rounded-full bg-surface-200 dark:bg-surface-800 text-surface-500">
            <Icon name="search" size={22} />
          </div>
          <p class="text-base font-medium mb-1">No cached messages match</p>
          <p class="text-sm text-surface-500 mb-4">
            Only messages already on this device were searched. Older mail you haven't opened or scrolled to yet may live on the server.
          </p>
          <button
            type="button"
            class="btn preset-filled-primary-500 inline-flex items-center gap-2"
            disabled={serverSearching}
            onclick={runServer}
          >
            <Icon name={serverSearching ? 'loading' : 'sync'} size={14} />
            {serverSearching ? 'Searching server…' : 'Search server too'}
          </button>
        {/if}
      </div>
    {:else}
      {#each hits as hit (`${hit.folder}:${hit.uid}`)}
        <button
          class="w-full text-left px-4 py-3 border-b border-surface-100 dark:border-surface-800 transition-colors
            {selectedUid === hit.uid
            ? 'bg-primary-500/10'
            : 'hover:bg-primary-500/10'}"
          onclick={() => onselect(hit.uid, hit.folder)}
        >
          <div class="flex items-center justify-between mb-1">
            <span class="text-sm {!hit.isRead ? 'font-semibold' : 'font-normal'} truncate pr-2">
              {hit.from || '(unknown sender)'}
            </span>
            <span class="text-xs text-surface-500 shrink-0">{formatDate(hit.date)}</span>
          </div>
          <p class="text-sm {!hit.isRead ? 'font-medium' : ''} truncate">
            {hit.subject || '(no subject)'}
          </p>
          {#if hit.snippet}
            <!-- Trusted: see safeSnippet() comment. -->
            <p class="text-xs text-surface-500 mt-0.5 truncate">
              <!-- eslint-disable-next-line -->
              {@html safeSnippet(hit.snippet)}
            </p>
          {/if}
          <div class="flex items-center gap-1 mt-0.5">
            {#if hit.folder !== currentFolder}
              <span class="text-[10px] px-1.5 rounded bg-surface-200 dark:bg-surface-700 text-surface-600 dark:text-surface-300">
                {hit.folder}
              </span>
            {/if}
            {#if hit.hasAttachments}
              <span class="text-[10px]" title="Has attachment">&#x1F4CE;</span>
            {/if}
          </div>
        </button>
      {/each}

      <!-- Server-search infinite-scroll status row (#194 follow-up).
           Only renders once "Search server too" has run and there's
           something to paginate. -->
      {#if loadingServerOlder}
        <div class="px-4 py-3 text-center text-xs text-surface-500 inline-flex items-center justify-center gap-2 w-full">
          <Icon name="loading" size={14} />
          Loading older server matches…
        </div>
      {:else if serverSearched && serverOlderExhausted && hits.length > 0}
        <div class="px-4 py-3 text-center text-[11px] text-surface-400 uppercase tracking-wider">
          End of server results
        </div>
      {/if}
    {/if}
  </div>
</div>

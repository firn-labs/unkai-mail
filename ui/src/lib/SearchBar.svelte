<script lang="ts">
  /**
   * SearchBar — search input with scope selector and
   * filter chips. Sits above the mail list. Triggers a parent
   * callback with the parsed query + filters so the caller can
   * decide how to render results (SearchResults vs MailList).
   *
   * Keyboard: Ctrl+F focuses the input.
   * Escape clears the query and blurs.
   */

  import { onMount, onDestroy } from 'svelte'
  import SearchInput from './SearchInput.svelte'
  import { displayFolderName } from './unifiedFolders'
  import { m } from '../paraglide/messages'

  export type SearchScope = {
    accountId?: string
    folder?: string
    limit?: number
  }

  export type SearchFilters = {
    unreadOnly?: boolean
    flaggedOnly?: boolean
    hasAttachment?: boolean
    dateFrom?: number | null
    dateTo?: number | null
  }

  /** Folder-scope choices surfaced in the dropdown. */
  type ScopeChoice = 'current' | 'allFolders'

  interface Props {
    /** Current folder so "this folder" scope can resolve. */
    currentFolder: string
    /** Current account id — search is always scoped to one account. */
    accountId: string
    /** Debounced fire: user typed or toggled. Null query + clean
     *  filters means "search is inactive, go back to mail list". */
    onsearch: (
      query: string,
      scope: SearchScope,
      filters: SearchFilters,
    ) => void
  }
  let { currentFolder, accountId, onsearch }: Props = $props()

  let query = $state('')
  let scope = $state<ScopeChoice>('current')
  let unread = $state(false)
  let flagged = $state(false)
  let hasAttachment = $state(false)
  let showHints = $state(false)
  let inputEl: HTMLInputElement | null = $state(null)

  // Debounce keystrokes — we don't want to hit the DB on every
  // character. 150ms keeps typing fluid while collapsing bursts.
  let debounceTimer: ReturnType<typeof setTimeout> | null = null

  function fireSearch() {
    const s: SearchScope = {
      accountId,
      folder: scope === 'current' ? currentFolder : undefined,
      limit: 200,
    }
    const f: SearchFilters = {
      unreadOnly: unread,
      flaggedOnly: flagged,
      hasAttachment,
    }
    onsearch(query, s, f)
  }

  function scheduleSearch() {
    if (debounceTimer) clearTimeout(debounceTimer)
    debounceTimer = setTimeout(fireSearch, 150)
  }

  // Reactive scheduler: any time `query` changes (user typing OR
  // the shared SearchInput's clear-X programmatically resetting
  // it), debounce a search.  Replaces the old `oninput` callback
  // which only fired on real keystrokes — the clear button in the
  // shared SearchInput assigns `value` directly, so an event-based
  // hook would have missed that path.
  let firstRun = true
  $effect(() => {
    // Touch the dependency so the effect re-runs on every change.
    void query
    if (firstRun) {
      firstRun = false
      return
    }
    scheduleSearch()
  })

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      if (query || unread || flagged || hasAttachment) {
        query = ''
        unread = false
        flagged = false
        hasAttachment = false
        fireSearch()
      }
      inputEl?.blur()
    } else if (e.key === 'Enter') {
      if (debounceTimer) clearTimeout(debounceTimer)
      fireSearch()
    }
  }

  function toggleChip(name: 'unread' | 'flagged' | 'hasAttachment') {
    if (name === 'unread') unread = !unread
    if (name === 'flagged') flagged = !flagged
    if (name === 'hasAttachment') hasAttachment = !hasAttachment
    fireSearch()
  }

  function onScopeChange() {
    fireSearch()
  }

  function insertOperator(op: string) {
    const suffix = query.endsWith(' ') || query.length === 0 ? '' : ' '
    query = `${query}${suffix}${op}`
    inputEl?.focus()
    scheduleSearch()
  }

  // Ctrl+F focuses the search input. We preventDefault so the browser's
  // built-in page-find dialog doesn't open on top of us.
  function handleGlobalKey(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') {
      e.preventDefault()
      inputEl?.focus()
      inputEl?.select()
    }
  }

  onMount(() => {
    window.addEventListener('keydown', handleGlobalKey)
  })
  onDestroy(() => {
    window.removeEventListener('keydown', handleGlobalKey)
    if (debounceTimer) clearTimeout(debounceTimer)
  })

  const hasAnyFilter = $derived(unread || flagged || hasAttachment)
  const isActive = $derived(query.trim().length > 0 || hasAnyFilter)

  // Operator hint rows — `insert` is what click-to-insert puts into
  // the query box, `example` is the monospace sample shown to the
  // user, `hint` the localised description (kept as the message fn
  // so the template call picks up the active locale).
  const searchTips = [
    { insert: 'from:', example: 'from:alice', hint: m.search_tip_from },
    {
      insert: 'subject:',
      example: 'subject:"weekly update"',
      hint: m.search_tip_subject,
    },
    {
      insert: 'has:attachment',
      example: 'has:attachment',
      hint: m.search_tip_has_attachment,
    },
    { insert: 'is:unread', example: 'is:unread', hint: m.search_tip_is_unread },
    { insert: 'after:', example: 'after:2026-01-31', hint: m.search_tip_after },
    {
      insert: 'before:',
      example: 'before:2026-01-31',
      hint: m.search_tip_before,
    },
    { insert: 'on:', example: 'on:2026-01-31', hint: m.search_tip_on },
    { insert: 'in:', example: 'in:Sent', hint: m.search_tip_in },
  ]
</script>

<div class="border-b border-surface-200 dark:border-surface-700 p-2 space-y-1.5" data-tour="search">
  <!-- Row 1: search input — uses the shared `SearchInput` so the
       magnifier / clear-X chrome stays in sync with every other
       "Search …" surface in the app.  The operator-hint dropdown
       rides along as a child snippet, anchored to SearchInput's
       relative wrapper. -->
  <SearchInput
    bind:inputEl
    bind:value={query}
    placeholder="Search mail  (Ctrl+F)"
    ariaLabel="Search mail"
    onkeydown={onKeydown}
    onfocus={() => (showHints = true)}
    onblur={() => setTimeout(() => (showHints = false), 150)}
  >
    {#if showHints && query.length === 0}
      <!-- Operator hint dropdown — shown while focused + empty -->
      <div
        class="absolute left-0 right-0 top-full mt-1 z-40 bg-white dark:bg-surface-900 border border-surface-200 dark:border-surface-700 rounded-md shadow-lg p-2 text-xs space-y-0.5"
      >
        <div class="font-semibold text-surface-500 mb-1">
          {m.search_tips_header()}
        </div>
        {#each searchTips as tip (tip.example)}
          <div
            role="button"
            tabindex="-1"
            class="cursor-pointer hover:bg-surface-100 dark:hover:bg-surface-800 px-1.5 py-0.5 rounded"
            onmousedown={(e) => {
              e.preventDefault()
              insertOperator(tip.insert)
            }}
          >
            <code class="font-mono">{tip.example}</code> — {tip.hint()}
          </div>
        {/each}
      </div>
    {/if}
  </SearchInput>

  <!-- Row 2: scope selector on its own line below the input. -->
  <div class="flex items-center gap-1.5">
    <label for="search-scope" class="text-xs text-surface-500 shrink-0">
      In:
    </label>
    <select
      id="search-scope"
      bind:value={scope}
      onchange={onScopeChange}
      class="select text-xs py-1 px-1.5 rounded-md flex-1 min-w-0"
      aria-label="Search scope"
      title="Search scope"
    >
      <option value="current">This folder ({displayFolderName(currentFolder)})</option>
      <option value="allFolders">All folders</option>
    </select>
  </div>

  <!-- Row 2: filter chips. Only shown when the search is active, to
       keep the idle mail-list header uncluttered. -->
  {#if isActive}
    <div class="flex flex-wrap items-center gap-1">
      <button
        type="button"
        class="chip text-xs px-2 py-0.5 rounded-full border transition
          {unread
          ? 'bg-primary-500/20 border-primary-500 text-primary-700 dark:text-primary-200'
          : 'border-surface-300 dark:border-surface-600 hover:bg-surface-100 dark:hover:bg-surface-800'}"
        onclick={() => toggleChip('unread')}
        aria-pressed={unread}
      >
        Unread
      </button>
      <button
        type="button"
        class="chip text-xs px-2 py-0.5 rounded-full border transition
          {flagged
          ? 'bg-primary-500/20 border-primary-500 text-primary-700 dark:text-primary-200'
          : 'border-surface-300 dark:border-surface-600 hover:bg-surface-100 dark:hover:bg-surface-800'}"
        onclick={() => toggleChip('flagged')}
        aria-pressed={flagged}
      >
        Flagged
      </button>
      <button
        type="button"
        class="chip text-xs px-2 py-0.5 rounded-full border transition
          {hasAttachment
          ? 'bg-primary-500/20 border-primary-500 text-primary-700 dark:text-primary-200'
          : 'border-surface-300 dark:border-surface-600 hover:bg-surface-100 dark:hover:bg-surface-800'}"
        onclick={() => toggleChip('hasAttachment')}
        aria-pressed={hasAttachment}
      >
        Has attachment
      </button>
    </div>
  {/if}
</div>

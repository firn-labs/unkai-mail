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
  import Icon from './Icon.svelte'
  import { displayFolderName } from './unifiedFolders'

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

  function onInput() {
    scheduleSearch()
  }

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
</script>

<div class="border-b border-surface-200 dark:border-surface-700 p-2 space-y-1.5">
  <!-- Row 1: search input (full width so the user can actually see
       what they're typing, even in a narrow mail-list column) -->
  <div class="relative w-full">
    <!-- Magnifier icon on the left -->
    <span
      class="absolute left-2 top-1/2 -translate-y-1/2 text-surface-400 pointer-events-none flex items-center"
      aria-hidden="true"
    >
      <Icon name="search" size={14} />
    </span>
    <input
      bind:this={inputEl}
      bind:value={query}
      oninput={onInput}
      onkeydown={onKeydown}
      onfocus={() => (showHints = true)}
      onblur={() => setTimeout(() => (showHints = false), 150)}
      type="text"
      placeholder="Search mail  (Ctrl+F)"
      class="input w-full pl-7 pr-8 py-1.5 text-sm rounded-md"
      aria-label="Search mail"
    />
    {#if query}
      <button
        type="button"
        title="Clear search"
        class="absolute right-2 top-1/2 -translate-y-1/2 text-surface-500 hover:text-surface-700 dark:hover:text-surface-200 text-xs"
        onclick={() => {
          query = ''
          fireSearch()
          inputEl?.focus()
        }}
        aria-label="Clear search"
      >
        &#x2715;
      </button>
    {/if}

    <!-- Operator hint dropdown — shown while focused + empty -->
    {#if showHints && query.length === 0}
      <div
        class="absolute left-0 right-0 top-full mt-1 z-40 bg-white dark:bg-surface-900 border border-surface-200 dark:border-surface-700 rounded-md shadow-lg p-2 text-xs space-y-0.5"
      >
        <div class="font-semibold text-surface-500 mb-1">Search tips</div>
        <div
          role="button"
          tabindex="-1"
          class="cursor-pointer hover:bg-surface-100 dark:hover:bg-surface-800 px-1.5 py-0.5 rounded"
          onmousedown={(e) => {
            e.preventDefault()
            insertOperator('from:')
          }}
        >
          <code class="font-mono">from:alice</code> — from a specific sender
        </div>
        <div
          role="button"
          tabindex="-1"
          class="cursor-pointer hover:bg-surface-100 dark:hover:bg-surface-800 px-1.5 py-0.5 rounded"
          onmousedown={(e) => {
            e.preventDefault()
            insertOperator('subject:')
          }}
        >
          <code class="font-mono">subject:"weekly update"</code> — subject
          contains
        </div>
        <div
          role="button"
          tabindex="-1"
          class="cursor-pointer hover:bg-surface-100 dark:hover:bg-surface-800 px-1.5 py-0.5 rounded"
          onmousedown={(e) => {
            e.preventDefault()
            insertOperator('has:attachment')
          }}
        >
          <code class="font-mono">has:attachment</code> — only with files
        </div>
        <div
          role="button"
          tabindex="-1"
          class="cursor-pointer hover:bg-surface-100 dark:hover:bg-surface-800 px-1.5 py-0.5 rounded"
          onmousedown={(e) => {
            e.preventDefault()
            insertOperator('is:unread')
          }}
        >
          <code class="font-mono">is:unread</code> — only unread
        </div>
      </div>
    {/if}
  </div>

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

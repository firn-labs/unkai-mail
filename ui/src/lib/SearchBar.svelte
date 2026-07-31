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
  let showHelp = $state(false)
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
      // While the syntax-help popup is open, Escape belongs to it —
      // the global handler closes it; don't also clear the query.
      if (showHelp) return
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
    showHelp = false
    inputEl?.focus()
    scheduleSearch()
  }

  // Ctrl+F focuses the search input. We preventDefault so the browser's
  // built-in page-find dialog doesn't open on top of us.  Escape closes
  // the syntax-help popup from anywhere while it's open.
  function handleGlobalKey(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') {
      e.preventDefault()
      inputEl?.focus()
      inputEl?.select()
    } else if (e.key === 'Escape' && showHelp) {
      e.preventDefault()
      showHelp = false
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

<div class="border-b glass-panel p-2 space-y-1.5" data-tour="search">
  <!-- Row 1: search input — uses the shared `SearchInput` so the
       magnifier / clear-X chrome stays in sync with every other
       "Search …" surface in the app — plus the syntax-help trigger.
       The operator documentation lives in a modal popup (below)
       rather than a focus-anchored dropdown, which used to overlap
       the mail list under the glass chrome. -->
  <div class="flex items-center gap-1.5">
    <SearchInput
      bind:inputEl
      bind:value={query}
      placeholder="Search mail  (Ctrl+F)"
      ariaLabel="Search mail"
      onkeydown={onKeydown}
      class="w-full flex-1 min-w-0"
    />
    <button
      type="button"
      class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center shrink-0"
      onclick={() => (showHelp = true)}
      title={m.search_help_button()}
      aria-label={m.search_help_button()}
    >
      <Icon name="help" size={14} />
    </button>
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
      class="select text-xs py-1 px-1.5 rounded-lg flex-1 min-w-0"
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
        class="chip text-xs px-2 py-0.5 rounded-full border transition duration-150 ease-out
          {unread
          ? 'bg-primary-500/20 border-primary-500 text-primary-700 dark:text-primary-200'
          : 'border-surface-300 dark:border-surface-600 hover:bg-primary-500/10'}"
        onclick={() => toggleChip('unread')}
        aria-pressed={unread}
      >
        Unread
      </button>
      <button
        type="button"
        class="chip text-xs px-2 py-0.5 rounded-full border transition duration-150 ease-out
          {flagged
          ? 'bg-primary-500/20 border-primary-500 text-primary-700 dark:text-primary-200'
          : 'border-surface-300 dark:border-surface-600 hover:bg-primary-500/10'}"
        onclick={() => toggleChip('flagged')}
        aria-pressed={flagged}
      >
        Flagged
      </button>
      <button
        type="button"
        class="chip text-xs px-2 py-0.5 rounded-full border transition duration-150 ease-out
          {hasAttachment
          ? 'bg-primary-500/20 border-primary-500 text-primary-700 dark:text-primary-200'
          : 'border-surface-300 dark:border-surface-600 hover:bg-primary-500/10'}"
        onclick={() => toggleChip('hasAttachment')}
        aria-pressed={hasAttachment}
      >
        Has attachment
      </button>
    </div>
  {/if}
</div>

{#if showHelp}
  <!-- Search-syntax documentation popup.  Standard modal shape:
       dimmed backdrop + glass-float card, dismissed by outside-click,
       Escape (global handler above), or the X button.  The operator
       rows stay clickable — picking one inserts it into the query
       and closes the popup. -->
  <div
    class="fixed inset-0 flex items-center justify-center bg-black/50"
    style="z-index: 50"
    role="dialog"
    aria-modal="true"
    aria-label={m.search_tips_header()}
    tabindex="-1"
    onmousedown={(e) => {
      if (e.target === e.currentTarget) showHelp = false
    }}
  >
    <div class="glass-float rounded-2xl w-[28rem] max-w-full p-5">
      <div class="flex items-start justify-between gap-2 mb-1">
        <h3 class="text-base font-semibold text-on-glass">
          {m.search_tips_header()}
        </h3>
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
          onclick={() => (showHelp = false)}
          title={m.search_help_close()}
          aria-label={m.search_help_close()}
        >
          <Icon name="close" size={14} />
        </button>
      </div>
      <p class="text-xs text-on-glass-muted mb-3">
        {m.search_help_intro()}
      </p>
      <div class="space-y-0.5 text-sm">
        {#each searchTips as tip (tip.example)}
          <button
            type="button"
            class="w-full text-left flex items-baseline gap-2 cursor-pointer hover:bg-primary-500/10 transition-colors duration-150 ease-out px-2 py-1 rounded-lg"
            onclick={() => insertOperator(tip.insert)}
          >
            <code class="font-mono text-xs shrink-0 text-on-glass">
              {tip.example}
            </code>
            <span class="text-xs text-on-glass-muted">{tip.hint()}</span>
          </button>
        {/each}
      </div>
    </div>
  </div>
{/if}

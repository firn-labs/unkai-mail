<script lang="ts">
  /**
   * SearchBar — slim single-row search above the mail list.
   *
   * The resting state is just the shared `SearchInput` plus a ghost
   * filter glyph.  The glyph opens an advanced-search popout — a
   * query *builder*: its structured fields (From / To / Subject /
   * dates / state toggles) don't run a hidden second search path,
   * they visibly compose the operator syntax (`from:alice
   * has:attachment after:2026-07-01`) into the search input itself.
   * One source of truth, and users passively learn the power-user
   * syntax by watching the panel write it.  Opening the panel with a
   * query already typed parses it back into the fields.
   *
   * The folder-scope selector lives in the popout too (it isn't part
   * of the query string — it maps to the `SearchScope` the parent
   * passes to the backend), as does the ghost help icon that opens
   * the operator-syntax documentation modal (#460).
   *
   * Keyboard: Ctrl+F focuses the input.  Escape closes the help
   * modal, then the popout, then clears the query — one layer at a
   * time.
   */

  import { onMount, onDestroy, tick, untrack } from 'svelte'
  import Icon from './Icon.svelte'
  import SearchInput from './SearchInput.svelte'
  import AddressAutocomplete from './AddressAutocomplete.svelte'
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

  /** Folder-scope choices surfaced in the popout's dropdown. */
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
  let showHelp = $state(false)
  let showAdvanced = $state(false)
  let inputEl: HTMLInputElement | null = $state(null)
  /** Outer container — input, glyph, and popout are one surface for
   *  outside-click purposes, so clicking back into the search input
   *  to refine the free text keeps the popout open. */
  let barEl: HTMLDivElement | null = $state(null)

  // ── Query builder: parse / rebuild the operator syntax ──────
  //
  // The panel's fields are projections of the query string.  All
  // structured state is $derived from `query` via `parseQuery`;
  // editing a field rebuilds the query via `buildQuery`.  The
  // round-trip is value-preserving (operands with spaces are
  // quoted, quoted operands are unquoted verbatim), so a field
  // re-rendering from the freshly parsed query shows exactly what
  // the user just typed — no cursor jumps, no sync flags.
  //
  // Operators the backend knows but the panel doesn't surface
  // (cc:, body:, in:, on:, and the parser's synonyms) are kept as
  // free text so a rebuild never drops them.

  type BuiltQuery = {
    text: string
    from: string
    to: string
    subject: string
    after: string
    before: string
    unread: boolean
    flagged: boolean
    attachment: boolean
  }

  function unquote(s: string): string {
    if (s.startsWith('"')) {
      return s.endsWith('"') && s.length > 1 ? s.slice(1, -1) : s.slice(1)
    }
    return s
  }

  function parseQuery(q: string): BuiltQuery {
    const out: BuiltQuery = {
      text: '',
      from: '',
      to: '',
      subject: '',
      after: '',
      before: '',
      unread: false,
      flagged: false,
      attachment: false,
    }
    const textParts: string[] = []
    // Token = optional `word:` prefix + operand, where the operand
    // is either a (possibly unterminated) quoted string or a bare
    // word.  Standalone quoted phrases and bare words are free text.
    const re = /([A-Za-z]+):("[^"]*(?:"|$)|[^\s"]*)|("[^"]*(?:"|$))|(\S+)/g
    let match: RegExpExecArray | null
    while ((match = re.exec(q)) !== null) {
      const [full, op, operand] = match
      if (op) {
        const val = unquote(operand ?? '')
        switch (op.toLowerCase()) {
          case 'from':
            out.from = val
            continue
          case 'to':
            out.to = val
            continue
          case 'subject':
            out.subject = val
            continue
          case 'after':
            out.after = val
            continue
          case 'before':
            out.before = val
            continue
          case 'is':
            if (val.toLowerCase() === 'unread') {
              out.unread = true
              continue
            }
            if (val.toLowerCase() === 'flagged') {
              out.flagged = true
              continue
            }
            break // is:read etc. — leave as free text
          case 'has':
            if (val.toLowerCase() === 'attachment') {
              out.attachment = true
              continue
            }
            break
        }
      }
      textParts.push(full)
    }
    out.text = textParts.join(' ')
    return out
  }

  function quoteOperand(v: string): string {
    // Quotes can't nest in the operator grammar — strip them, then
    // wrap iff the operand contains whitespace.
    const clean = v.replace(/"/g, '')
    return /\s/.test(clean) ? `"${clean}"` : clean
  }

  function buildQuery(p: BuiltQuery): string {
    const parts: string[] = []
    if (p.text.trim()) parts.push(p.text.trim())
    const op = (key: string, v: string) => {
      if (v) parts.push(`${key}:${quoteOperand(v)}`)
    }
    op('from', p.from)
    op('to', p.to)
    op('subject', p.subject)
    op('after', p.after)
    op('before', p.before)
    if (p.unread) parts.push('is:unread')
    if (p.flagged) parts.push('is:flagged')
    if (p.attachment) parts.push('has:attachment')
    return parts.join(' ')
  }

  const parsed = $derived(parseQuery(query))

  function updateField(patch: Partial<BuiltQuery>) {
    query = buildQuery({ ...parsed, ...patch })
  }

  // From / To need local mirrors because `AddressAutocomplete`
  // two-way binds its value (typing AND picking a contact both
  // assign it).  The two effects converge instead of looping: a
  // field edit rebuilds `query`, whose re-parse then equals the
  // field, and vice versa — each guard sees equality and stops.
  let advFrom = $state('')
  let advTo = $state('')
  $effect(() => {
    const p = parsed
    untrack(() => {
      if (p.from !== advFrom) advFrom = p.from
      if (p.to !== advTo) advTo = p.to
    })
  })
  $effect(() => {
    const from = advFrom
    const to = advTo
    untrack(() => {
      if (from !== parsed.from || to !== parsed.to) {
        updateField({ from, to })
      }
    })
  })

  /** Any structured criterion active — tints the filter glyph so a
   *  closed popout still signals "this search is filtered". */
  const advActive = $derived(
    !!(
      parsed.from
      || parsed.to
      || parsed.subject
      || parsed.after
      || parsed.before
      || parsed.unread
      || parsed.flagged
      || parsed.attachment
    ) || scope === 'allFolders',
  )

  // ── Debounced search dispatch ───────────────────────────────
  // 150ms keeps typing fluid while collapsing bursts.  Everything
  // funnels through `query` — panel field edits rebuild it, so one
  // reactive scheduler covers typing and building alike.
  let debounceTimer: ReturnType<typeof setTimeout> | null = null

  function fireSearch() {
    const s: SearchScope = {
      accountId,
      folder: scope === 'current' ? currentFolder : undefined,
      limit: 200,
    }
    // State filters (unread / flagged / attachment) travel as
    // operators inside the query string now — the builder writes
    // is:unread etc. and the backend parser applies them.  The
    // filters argument stays for the parent's contract but is
    // always inactive.
    onsearch(query, s, {})
  }

  function scheduleSearch() {
    if (debounceTimer) clearTimeout(debounceTimer)
    debounceTimer = setTimeout(fireSearch, 150)
  }

  // Reactive scheduler: any time `query` changes (user typing, the
  // shared SearchInput's clear-X programmatically resetting it, or
  // the builder rebuilding it), debounce a search.
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
      // Escape peels one layer at a time: help modal (global
      // handler), then the popout, then the query itself.
      if (showHelp) return
      if (showAdvanced) {
        showAdvanced = false
        return
      }
      if (query) {
        query = ''
        fireSearch()
      }
      inputEl?.blur()
    } else if (e.key === 'Enter') {
      if (debounceTimer) clearTimeout(debounceTimer)
      fireSearch()
    }
  }

  function onScopeChange() {
    fireSearch()
  }

  /** Commit from the popout: close it and search NOW.  Wired to the
   *  panel's search button and to contact picks in the From / To
   *  autocomplete.  `tick()` first — a pick lands in the query via
   *  the advFrom/advTo → query effect chain, so firing synchronously
   *  would search the stale query. */
  async function commitSearch() {
    showAdvanced = false
    await tick()
    if (debounceTimer) clearTimeout(debounceTimer)
    fireSearch()
    inputEl?.focus()
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
  // the help modal first, then the advanced popout — unless an inner
  // widget (e.g. the contact-autocomplete dropdown) already consumed
  // the keypress via preventDefault.
  function handleGlobalKey(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') {
      e.preventDefault()
      inputEl?.focus()
      inputEl?.select()
    } else if (e.key === 'Escape' && !e.defaultPrevented) {
      if (showHelp) {
        e.preventDefault()
        showHelp = false
      } else if (showAdvanced) {
        showAdvanced = false
      }
    }
  }

  // Outside-click dismissal for the popout — the standard idiom:
  // listener registered inside an $effect keyed on the open state,
  // one-tick delay so the opening click doesn't instantly close it.
  $effect(() => {
    if (!showAdvanced) return
    const handler = (e: MouseEvent) => {
      const t = e.target as Node | null
      if (t && barEl?.contains(t)) return
      showAdvanced = false
    }
    const id = setTimeout(
      () => document.addEventListener('mousedown', handler),
      0,
    )
    return () => {
      clearTimeout(id)
      document.removeEventListener('mousedown', handler)
    }
  })

  onMount(() => {
    window.addEventListener('keydown', handleGlobalKey)
  })
  onDestroy(() => {
    window.removeEventListener('keydown', handleGlobalKey)
    if (debounceTimer) clearTimeout(debounceTimer)
  })

  // Operator hint rows for the syntax-help modal — `insert` is what
  // click-to-insert puts into the query box, `example` the monospace
  // sample, `hint` the localised description (kept as the message fn
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

  const ghostBtn =
    'p-1.5 rounded-lg transition-colors duration-150 ease-out '
    + 'hover:text-primary-500 hover:bg-primary-500/10 shrink-0'
</script>

<!-- z-30: the glass bar's backdrop-filter creates a stacking context
     with z-index auto, which paints at the same level as the mail
     list's positioned rows (`relative` on every MailList row) — and
     the list, being later in DOM order, would win.  The popout's own
     z-index is trapped INSIDE this context and can't fix that, so
     the elevation has to sit here, on the context itself.  30 keeps
     us under modals and fixed context menus (z-50). -->
<div
  bind:this={barEl}
  class="border-b glass-panel p-2 relative z-30"
  data-tour="search"
>
  <!-- Resting state: one slim row.  Scope, state toggles, and the
       structured fields all live in the popout so the idle mail-list
       header stays quiet. -->
  <div class="flex items-center gap-1">
    <SearchInput
      bind:inputEl
      bind:value={query}
      placeholder={m.search_placeholder()}
      ariaLabel={m.search_placeholder()}
      onkeydown={onKeydown}
      class="w-full flex-1 min-w-0"
    />
    <button
      type="button"
      class="{ghostBtn} {showAdvanced || advActive
        ? 'text-primary-500'
        : 'text-on-glass-muted'}"
      onclick={() => (showAdvanced = !showAdvanced)}
      aria-expanded={showAdvanced}
      title={m.search_adv_toggle()}
      aria-label={m.search_adv_toggle()}
    >
      <Icon name="filter" size={16} />
    </button>
  </div>

  {#if showAdvanced}
    <!-- Advanced-search popout / query builder.  Docked flush under
         the bar (no gap, square top corners) so it reads as a drawer
         sliding out of the search surface, not a free-floating card;
         the bar's own border-b is the divider between the two.  It
         deliberately floats over the mail list — an intentional
         surface the user opened, unlike the retired focus-triggered
         hint dropdown (#460).  Deliberately opaque, not .glass-float:
         it renders inside the glass search bar (stacking a second
         backdrop-filter layer is off-limits), and a translucent panel
         over the mail list is unreadable — live results can wait
         beneath it. -->
    <div
      class="absolute left-0 right-0 top-full z-40 bg-surface-50 dark:bg-surface-900 border border-t-0 border-surface-300 dark:border-surface-600 rounded-b-xl shadow-lg p-3 space-y-2"
      aria-label={m.search_adv_toggle()}
    >
      <!-- From / To stacked full-width: names and addresses need the
           room, and the autocomplete dropdown anchors better to a
           wide field. -->
      <div>
        <label
          for="search-adv-from"
          class="block text-xs text-surface-500 mb-0.5"
        >
          {m.search_adv_from()}
        </label>
        <AddressAutocomplete
          id="search-adv-from"
          bind:value={advFrom}
          pickMode="replace-address"
          inputClass="input w-full px-2 py-1 text-sm rounded-lg"
          placeholder={m.search_adv_person_placeholder()}
          onpick={() => void commitSearch()}
        />
      </div>
      <div>
        <label
          for="search-adv-to"
          class="block text-xs text-surface-500 mb-0.5"
        >
          {m.search_adv_to()}
        </label>
        <AddressAutocomplete
          id="search-adv-to"
          bind:value={advTo}
          pickMode="replace-address"
          inputClass="input w-full px-2 py-1 text-sm rounded-lg"
          placeholder={m.search_adv_person_placeholder()}
          onpick={() => void commitSearch()}
        />
      </div>

      <div>
        <label
          for="search-adv-subject"
          class="block text-xs text-surface-500 mb-0.5"
        >
          {m.search_adv_subject()}
        </label>
        <input
          id="search-adv-subject"
          type="text"
          class="input w-full px-2 py-1 text-sm rounded-lg"
          value={parsed.subject}
          oninput={(e) => updateField({ subject: e.currentTarget.value })}
        />
      </div>

      <div class="grid grid-cols-2 gap-2">
        <div>
          <label
            for="search-adv-after"
            class="block text-xs text-surface-500 mb-0.5"
          >
            {m.search_adv_after()}
          </label>
          <input
            id="search-adv-after"
            type="date"
            class="input w-full px-2 py-1 text-sm rounded-lg"
            value={parsed.after}
            onchange={(e) => updateField({ after: e.currentTarget.value })}
          />
        </div>
        <div>
          <label
            for="search-adv-before"
            class="block text-xs text-surface-500 mb-0.5"
          >
            {m.search_adv_before()}
          </label>
          <input
            id="search-adv-before"
            type="date"
            class="input w-full px-2 py-1 text-sm rounded-lg"
            value={parsed.before}
            onchange={(e) => updateField({ before: e.currentTarget.value })}
          />
        </div>
      </div>

      <div class="flex flex-wrap items-center gap-1">
        <button
          type="button"
          class="chip text-xs px-2 py-0.5 rounded-full border transition duration-150 ease-out
            {parsed.unread
            ? 'bg-primary-500/20 border-primary-500 text-primary-700 dark:text-primary-200'
            : 'border-surface-300 dark:border-surface-600 hover:bg-primary-500/10'}"
          onclick={() => updateField({ unread: !parsed.unread })}
          aria-pressed={parsed.unread}
        >
          {m.search_filter_unread()}
        </button>
        <button
          type="button"
          class="chip text-xs px-2 py-0.5 rounded-full border transition duration-150 ease-out
            {parsed.flagged
            ? 'bg-primary-500/20 border-primary-500 text-primary-700 dark:text-primary-200'
            : 'border-surface-300 dark:border-surface-600 hover:bg-primary-500/10'}"
          onclick={() => updateField({ flagged: !parsed.flagged })}
          aria-pressed={parsed.flagged}
        >
          {m.search_filter_flagged()}
        </button>
        <button
          type="button"
          class="chip text-xs px-2 py-0.5 rounded-full border transition duration-150 ease-out
            {parsed.attachment
            ? 'bg-primary-500/20 border-primary-500 text-primary-700 dark:text-primary-200'
            : 'border-surface-300 dark:border-surface-600 hover:bg-primary-500/10'}"
          onclick={() => updateField({ attachment: !parsed.attachment })}
          aria-pressed={parsed.attachment}
        >
          {m.search_filter_attachment()}
        </button>
      </div>

      <div class="flex items-center gap-1.5">
        <label
          for="search-scope"
          class="text-xs text-surface-500 shrink-0"
        >
          {m.search_scope_label()}
        </label>
        <select
          id="search-scope"
          bind:value={scope}
          onchange={onScopeChange}
          class="select text-xs py-1 px-1.5 rounded-lg flex-1 min-w-0"
          title={m.search_scope_title()}
        >
          <option value="current">
            {m.search_scope_current({
              folder: displayFolderName(currentFolder),
            })}
          </option>
          <option value="allFolders">{m.search_scope_all()}</option>
        </select>
        <button
          type="button"
          class="{ghostBtn} text-surface-500"
          onclick={() => (showHelp = true)}
          title={m.search_help_button()}
          aria-label={m.search_help_button()}
        >
          <Icon name="help" size={16} />
        </button>
        <button
          type="button"
          class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center"
          onclick={() => void commitSearch()}
          title={m.search_adv_run()}
          aria-label={m.search_adv_run()}
        >
          <Icon name="search" size={14} />
        </button>
      </div>
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

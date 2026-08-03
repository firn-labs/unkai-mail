<script lang="ts">
  /**
   * AddressAutocomplete — a comma/semicolon-separated address input
   * with a dropdown of matching contacts.
   *
   * # Why a wrapper around <input>
   *
   * The compose form has three recipient fields (To / Cc / Bcc) and we
   * want the same behaviour on all of them: type `ali`, see Alice
   * Example pop up with her photo thumbnail, arrow-down + Enter fills
   * in `Alice Example <alice@example.com>, ` and keeps the caret in
   * the field ready for the next address. Factoring it into one
   * component beats duplicating the plumbing three times.
   *
   * The mail search's advanced panel reuses the same dropdown for its
   * From / To builder fields via `pickMode="replace-address"` — same
   * contacts, same avatars, but a pick commits a single bare email
   * address instead of appending an RFC-formatted recipient.
   *
   * # What the backend does vs. what we do
   *
   * The Rust `search_contacts` command does the LIKE query over the
   * local cache and returns fully hydrated Contact rows. We add UI
   * polish on top: debouncing (so we don't hit the IPC bridge on
   * every keystroke), token parsing (the query is only the text
   * after the last `,` or `;`), and keyboard navigation.
   *
   * Photos render via the `contact-photo://` URI scheme — the
   * webview fetches the bytes straight from the Rust cache, so the
   * dropdown payload stays tiny (no JSON byte-array bloat) and the
   * browser caches per-id automatically.
   */

  import { convertFileSrc, invoke } from '@tauri-apps/api/core'
  import { onDestroy } from 'svelte'
  import EmailKindChip from './EmailKindChip.svelte'

  interface ContactEmail {
    kind: string
    value: string
  }
  interface ContactPhone {
    kind: string
    value: string
  }
  interface Contact {
    id: string
    nextcloud_account_id: string
    display_name: string
    email: ContactEmail[]
    phone: ContactPhone[]
    organization: string | null
    photo_mime: string | null
  }

  interface Props {
    /** The full field value (`"alice@x, bob@y"`). Two-way via bind. */
    value: string
    placeholder?: string
    id?: string
    /** How picking a suggestion commits into the field:
     *  - `'append-rfc'` (default, the compose behaviour): append
     *    `"Display Name" <addr>, ` after the existing addresses so
     *    the user keeps typing the next recipient.
     *  - `'replace-address'`: the field holds ONE value and a pick
     *    replaces it with the bare email address — used by the mail
     *    search's From / To builder fields, where the value becomes
     *    a `from:` / `to:` operator operand (bare address matches
     *    the FTS index; the RFC form's quotes and angle brackets
     *    would fight the query grammar).  Mailing-list suggestions
     *    are hidden in this mode — a list expands to many addresses,
     *    which a single-operand operator can't hold. */
    pickMode?: 'append-rfc' | 'replace-address'
    /** Class for the underlying `<input>` — override when the field
     *  sits in a denser layout than the compose form. */
    inputClass?: string
  }
  let {
    value = $bindable(''),
    placeholder = '',
    id = '',
    pickMode = 'append-rfc',
    inputClass = 'input w-full px-3 py-2 text-sm rounded-lg',
  }: Props = $props()

  // ── Dropdown state ─────────────────────────────────────────
  /** A suggestion row — either an individual contact (`kind:
   *  'contact'`) or a group / mailing list (#133, #113), in
   *  which case selecting it expands every member into the
   *  field rather than inserting a single address. */
  type Suggestion =
    | {
        kind: 'contact'
        contact: Contact
        /** Specific email of the contact this row represents.
         *  Each contact expands into one suggestion per email
         *  address so users can pick a Home vs Work address
         *  directly from the dropdown. */
        email: { kind: string; value: string }
      }
    | { kind: 'list'; list: MailingListSuggestion }
  interface MailingListSuggestion {
    id: string
    /** `category` | `team` | `manual` — drives the pill colour. */
    source: 'category' | 'team' | 'manual'
    name: string
    members: { displayName: string; email: string }[]
    hiddenFromAutocomplete: boolean
    emoji: string | null
  }
  let suggestions = $state<Suggestion[]>([])
  let open = $state(false)
  let activeIndex = $state(0)
  let inputEl: HTMLInputElement | undefined = $state()
  /** All non-hidden groups, fetched once and refreshed when
   *  `list_contact_groups` is available — we filter client-side
   *  on every keystroke so the dropdown stays snappy. */
  let allLists = $state<MailingListSuggestion[]>([])
  // svelte-ignore state_referenced_locally — `pickMode` is a
  // configuration prop that never changes after mount; reading it
  // once at init to gate the fetch is deliberate.
  if (pickMode !== 'replace-address') {
    // Lists never surface in replace-address mode, so skip the IPC.
    void invoke<MailingListSuggestion[]>('list_mailing_lists')
      .then((rows) => {
        // Backend already excludes "Use as mailing list = off"
        // categories; the per-row hide swatch (manual / team
        // sources) is the remaining filter we apply here.
        allLists = rows.filter((m) => !m.hiddenFromAutocomplete)
      })
      .catch((e) => console.warn('list_mailing_lists failed', e))
  }

  // 150ms debounce keeps the UI snappy without firing a DB query on
  // every keystroke of a fast typer. The timer is a setTimeout handle
  // that we cancel-and-restart on each input event.
  let debounceTimer: number | null = null
  const DEBOUNCE_MS = 150
  const LIMIT = 8  // max dropdown rows; bigger lists scroll

  /** Extract the trailing token (the part after the last , or ;). */
  function currentToken(s: string): { token: string; prefix: string } {
    // Find the last separator; everything after is what the user is
    // actively typing. Everything before (+ separator + trailing
    // whitespace) is preserved verbatim when we commit a selection.
    const match = s.match(/^(.*[,;]\s*)?(.*)$/s)
    const prefix = match?.[1] ?? ''
    const token = (match?.[2] ?? '').trimStart()
    return { token, prefix }
  }

  async function runSearch(query: string) {
    if (query.length < 2) {
      suggestions = []
      open = false
      return
    }
    try {
      const rows = await invoke<Contact[]>('search_contacts', {
        query,
        limit: LIMIT,
      })
      // Match groups client-side from the cached `allGroups` —
      // they're typically a handful per user, so a substring
      // scan beats round-tripping a dedicated IPC.
      const q = query.toLowerCase()
      const listHits =
        pickMode === 'replace-address'
          ? []
          : allLists
              .filter((m) => m.name.toLowerCase().includes(q))
              .slice(0, LIMIT)
      // Mailing-list hits stay first (typing a list name is
      // almost always intent to address the bundle), then
      // individual contacts.  The standard mail-client
      // autocomplete ranking.
      // Expand each contact into one suggestion per email
      // address so a contact with both home and work emails
      // shows up twice — the user can pick the address they
      // want without first selecting the contact and then
      // editing.
      const contactSuggestions: Suggestion[] = []
      for (const c of rows) {
        const emails = c.email.filter((e) => e.value.length > 0)
        if (emails.length === 0) continue
        for (const e of emails) {
          contactSuggestions.push({ kind: 'contact', contact: c, email: e })
        }
      }
      const merged: Suggestion[] = [
        ...listHits.map((m) => ({ kind: 'list' as const, list: m })),
        ...contactSuggestions,
      ]
      suggestions = merged.slice(0, LIMIT)
      activeIndex = 0
      open = suggestions.length > 0
    } catch (e) {
      // Autocomplete is a nice-to-have — never surface errors here,
      // just collapse the dropdown silently.
      console.warn('search_contacts failed', e)
      suggestions = []
      open = false
    }
  }

  function onInput() {
    // In replace-address mode the whole value is one query — there
    // are no comma-separated tokens to peel apart.
    const token =
      pickMode === 'replace-address'
        ? value.trim()
        : currentToken(value).token
    if (debounceTimer !== null) window.clearTimeout(debounceTimer)
    debounceTimer = window.setTimeout(() => runSearch(token), DEBOUNCE_MS)
  }

  /**
   * Format a contact + chosen email into an RFC-style address.
   * Prefer `"Display Name" <addr@x>` when a display name is
   * present so the SMTP send path gets a friendly From header;
   * bare address if not.
   */
  function formatAddress(c: Contact, addr: string): string {
    if (!addr) return ''
    if (c.display_name && c.display_name !== addr) {
      const safe = c.display_name.replace(/"/g, '\\"')
      return `"${safe}" <${addr}>`
    }
    return addr
  }

  function pickContact(c: Contact, email: string) {
    if (pickMode === 'replace-address') {
      if (!email) return
      value = email
      suggestions = []
      open = false
      inputEl?.focus()
      return
    }
    const { prefix } = currentToken(value)
    const formatted = formatAddress(c, email)
    if (!formatted) return
    // Insert the selected address and a trailing `, ` so the user can
    // keep typing the next one without extra keystrokes.
    value = `${prefix}${formatted}, `
    suggestions = []
    open = false
    // Restore focus in case the click stole it.
    inputEl?.focus()
  }

  /** Expand a mailing list — every member's email lands in
   *  the field as its own RFC-shaped address, members without
   *  an email are silently skipped (they wouldn't survive an
   *  SMTP send anyway). */
  function pickList(m: MailingListSuggestion) {
    const { prefix } = currentToken(value)
    const formatted = m.members
      .filter((mm) => mm.email)
      .map((mm) => {
        if (mm.displayName && mm.displayName !== mm.email) {
          const safe = mm.displayName.replace(/"/g, '\\"')
          return `"${safe}" <${mm.email}>`
        }
        return mm.email
      })
      .join(', ')
    if (!formatted) return
    value = `${prefix}${formatted}, `
    suggestions = []
    open = false
    inputEl?.focus()
  }

  function pick(s: Suggestion) {
    if (s.kind === 'contact') pickContact(s.contact, s.email.value)
    else pickList(s.list)
  }

  function onKeydown(e: KeyboardEvent) {
    if (!open || suggestions.length === 0) return
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      activeIndex = (activeIndex + 1) % suggestions.length
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      activeIndex = (activeIndex - 1 + suggestions.length) % suggestions.length
    } else if (e.key === 'Enter') {
      // Intercept Enter only when dropdown is open — otherwise Enter
      // should fall through to any form submit handler.
      e.preventDefault()
      pick(suggestions[activeIndex])
    } else if (e.key === 'Escape') {
      e.preventDefault()
      open = false
    } else if (e.key === 'Tab') {
      // Tab behaves like Enter for autocomplete — lets power users
      // blast through a list of names quickly.
      if (suggestions.length > 0) {
        e.preventDefault()
        pick(suggestions[activeIndex])
      }
    }
  }

  function onBlur() {
    // Close on a tick so a click on a suggestion can fire first;
    // onmousedown on the suggestion will have already committed.
    setTimeout(() => {
      open = false
    }, 120)
  }

  /**
   * URL for `<img src>` against the custom `contact-photo://` URI
   * scheme. Returns `null` for contacts with no photo so callers
   * can render the initials placeholder. The browser caches per-id
   * so a contact that pops in and out of the dropdown only fetches
   * its avatar once.
   */
  function photoUrl(c: Contact): string | null {
    if (!c.photo_mime) return null
    return convertFileSrc(c.id, 'contact-photo')
  }

  function initials(name: string): string {
    const parts = name.trim().split(/\s+/).filter(Boolean)
    if (parts.length === 0) return '?'
    if (parts.length === 1) return parts[0][0].toUpperCase()
    return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase()
  }

  onDestroy(() => {
    if (debounceTimer !== null) window.clearTimeout(debounceTimer)
  })
</script>

<div class="relative flex-1">
  <input
    {id}
    {placeholder}
    bind:this={inputEl}
    bind:value
    class={inputClass}
    aria-label={placeholder}
    oninput={onInput}
    onkeydown={onKeydown}
    onblur={onBlur}
    onfocus={() => {
      if (suggestions.length > 0) open = true
    }}
    autocomplete="off"
  />

  {#if open && suggestions.length > 0}
    <ul
      class="absolute left-0 right-0 top-full mt-1 z-50 max-h-72 overflow-y-auto
             bg-surface-50 dark:bg-surface-900 border border-surface-300
             dark:border-surface-700 rounded-lg shadow-lg"
      role="listbox"
    >
      {#each suggestions as s, i (s.kind === 'contact' ? `${s.contact.id}::${s.email.value}` : s.list.id)}
        <li
          role="option"
          aria-selected={i === activeIndex}
          class="flex items-center gap-3 px-3 py-2 cursor-pointer text-sm
                 {i === activeIndex ? 'bg-primary-500/15' : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
          onmousedown={(e) => {
            // stopPropagation: pick() unmounts this row synchronously,
            // so any document-level outside-click listener (the mail
            // search's advanced panel) would later see a detached
            // target and misread the pick as an outside click.
            e.preventDefault()
            e.stopPropagation()
            pick(s)
          }}
          onmouseenter={() => (activeIndex = i)}
        >
          {#if s.kind === 'contact'}
            {@const c = s.contact}
            {@const url = photoUrl(c)}
            {#if url}
              <img
                src={url}
                alt=""
                loading="lazy"
                class="w-8 h-8 rounded-full object-cover flex-shrink-0"
              />
            {:else}
              <div class="w-8 h-8 rounded-full bg-surface-300 dark:bg-surface-700
                          flex items-center justify-center text-xs font-semibold flex-shrink-0">
                {initials(c.display_name)}
              </div>
            {/if}
            <div class="flex-1 min-w-0">
              <p class="font-medium truncate">{c.display_name}</p>
              <p class="text-xs text-surface-500 truncate flex items-center gap-1.5">
                <EmailKindChip kind={s.email.kind} />
                <span class="truncate">{s.email.value}</span>
                {#if c.organization}<span class="shrink-0">· {c.organization}</span>{/if}
              </p>
            </div>
          {:else}
            {@const m = s.list}
            {@const sendable = m.members.filter((mm) => mm.email).length}
            {@const pillCls = m.source === 'category'
              ? 'bg-primary-500/20 text-primary-600 dark:text-primary-300'
              : m.source === 'team'
                ? 'bg-surface-300 dark:bg-surface-600 text-surface-700 dark:text-surface-200'
                : 'bg-success-500/20 text-success-600 dark:text-success-300'}
            {@const icon = m.emoji || (m.source === 'category' ? '🏷️' : m.source === 'team' ? '⚡' : '📨')}
            <div class="w-8 h-8 rounded-full bg-primary-500/20 text-primary-600 dark:text-primary-300
                        flex items-center justify-center text-base font-semibold flex-shrink-0">
              {icon}
            </div>
            <div class="flex-1 min-w-0">
              <p class="font-medium truncate flex items-center gap-2">
                <span class="truncate">{m.name}</span>
                <span class="text-[10px] uppercase tracking-wider font-semibold px-1 py-px rounded {pillCls}">
                  {m.source}
                </span>
              </p>
              <p class="text-xs text-surface-500 truncate">
                {sendable} member{sendable === 1 ? '' : 's'} with email
              </p>
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>

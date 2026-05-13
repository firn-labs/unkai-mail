<script lang="ts">
  /**
   * NotesMarkdownEditor — CodeMirror 6 markdown editor for the Notes
   * UI (#138).
   *
   * Plain markdown source with grammar-aware syntax highlighting +
   * an optional preview pane that renders via `marked`.  Real
   * markdown round-trip end-to-end: what the user types is what
   * goes to the Nextcloud Notes API, no Tiptap-style HTML
   * intermediate.
   *
   * # Why CodeMirror 6 and not Tiptap
   *
   * The user explicitly asked for "real markdown" in #138.  Tiptap
   * stores ProseMirror nodes and would need a serializer to round-
   * trip through markdown — fine for casual prose but mildly lossy
   * on tables, custom HTML, advanced lists.  CodeMirror is honest
   * to the on-disk format and handles long documents well.
   *
   * # Preview
   *
   * Toggleable side-by-side preview pane — clicking the preview
   * button flips a flag on the parent.  Rendering uses `marked`
   * with no sanitiser because the source is the user's own notes,
   * already trusted.  We don't run inbound third-party markdown
   * here; if that ever changes (e.g. shared notes from another
   * user) wrap `marked.parse()` in DOMPurify.
   */

  import { onDestroy, onMount } from 'svelte'
  import { EditorState, Compartment } from '@codemirror/state'
  import { EditorView, keymap, lineNumbers, highlightActiveLine } from '@codemirror/view'
  import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands'
  import {
    bracketMatching,
    foldGutter,
    indentOnInput,
    syntaxHighlighting,
    defaultHighlightStyle,
  } from '@codemirror/language'
  import { markdown } from '@codemirror/lang-markdown'
  import { marked } from 'marked'
  import { invoke, convertFileSrc } from '@tauri-apps/api/core'
  import {
    createMentionExtension,
    insertMention,
    type MentionContext,
  } from './notesMentionExtension'
  import NotesMentionPicker, {
    type ContactItem,
    type MailItem,
    type PickerItem,
  } from './NotesMentionPicker.svelte'

  interface Props {
    /** Markdown source — `bind:value` from the parent.  Updates
     *  flow both directions: external programmatic changes
     *  (loading a different note) reset the editor's contents,
     *  and user typing flows back through the dispatch handler. */
    value: string
    /** Fires on every user-triggered edit.  The parent uses this
     *  to schedule the auto-save debounce; nothing else. */
    onchange?: (next: string) => void
    /** Toggle for the side-by-side rendered preview. */
    showPreview?: boolean
    /** Placeholder shown when the document is empty. */
    placeholder?: string
    /** Active mail account, used to scope the `/mail` IMAP-server
     *  fallback (#260).  When `null`, the picker still works against
     *  the cross-folder FTS5 cache — the server fallback just
     *  no-ops. */
    accountId?: string | null
    /** `mail://acc/folder/uid` link click handler (#260).  Called
     *  when the user clicks a Nimbus-internal mail reference in the
     *  rendered preview pane.  When absent, link clicks fall
     *  through to the browser's default (which silently no-ops on
     *  the unregistered scheme). */
    onopenmail?: (accountId: string, folder: string, uid: number) => void
  }
  let {
    value = $bindable(''),
    onchange,
    showPreview = false,
    placeholder = 'Start writing — markdown is preserved on the server.',
    accountId = null,
    onopenmail,
  }: Props = $props()

  let host: HTMLDivElement | undefined = $state()
  let view: EditorView | undefined = $state()
  let picker: NotesMentionPicker | undefined = $state()

  // ── Mention picker state (#260) ────────────────────────────────
  //
  // `mentionCtx` is the live trigger position from the CM6 plugin;
  // `pickerItems` is the current resolved row list.  Two fields
  // because the trigger updates synchronously on every keystroke
  // but the data fetch (debounced + async) lags behind.  The popup
  // stays mounted while a context exists so the "Searching…" state
  // is visible while we wait.
  let mentionCtx = $state<MentionContext | null>(null)
  let pickerItems = $state<PickerItem[]>([])
  let pickerLoading = $state(false)
  /** Per-query nonce so a slow `search_emails` round-trip whose
   *  query is no longer current can't clobber a fresher response.
   *  Bumped on every `mentionCtx` change; checked on every async
   *  resolve before writing into `pickerItems`. */
  let pickerQueryToken = 0
  /** Debounce timer for `/mail` searches.  The contact filter
   *  runs against an in-memory list so it doesn't need one. */
  let mailDebounce: ReturnType<typeof setTimeout> | null = null

  // ── Contact source (#260) ──────────────────────────────────────
  //
  // Same eager-load posture as Compose's `@` picker: pull every
  // contact once on mount, filter locally on each query.  Failure
  // is non-fatal — the picker still works for the `/mail` flow.
  interface ContactKindValue {
    kind: string
    value: string
  }
  interface ContactRow {
    id: string
    nextcloud_account_id: string
    display_name: string
    email: ContactKindValue[]
    phone: ContactKindValue[]
    organization: string | null
    photo_mime: string | null
  }
  let allContacts = $state<ContactRow[]>([])
  $effect(() => {
    void invoke<ContactRow[]>('get_contacts')
      .then((rows) => {
        allContacts = rows
      })
      .catch((e) => {
        console.warn('NotesMarkdownEditor: get_contacts failed', e)
      })
  })

  // ── Mail search wire shape ─────────────────────────────────────
  // Matches the SearchHit DTO from `search_emails` (camelCase via
  // serde rename).  Kept inline because the shape is small and
  // doesn't warrant a separate types module.
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
  interface IMAPEnvelope {
    account_id?: string
    folder?: string
    uid: number
    from: string
    subject: string
    date: string
    is_read?: boolean
  }

  /** Compartments let us reconfigure individual extensions
   *  (theme, read-only, etc.) without rebuilding the whole
   *  editor state.  We don't use them yet but they're free. */
  const themeCompartment = new Compartment()

  /** Skeleton-flavoured CM6 theme.  Light + dark variants by
   *  flipping CSS custom properties so `@media (prefers-color-
   *  scheme)` and the app's `data-mode` switch both Just Work. */
  const editorTheme = EditorView.theme({
    '&': {
      height: '100%',
      fontSize: '0.875rem',
      backgroundColor: 'transparent',
    },
    '.cm-scroller': {
      fontFamily:
        'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
      lineHeight: '1.6',
      padding: '0.5rem 0',
    },
    '.cm-content': {
      padding: '0.25rem 1.25rem',
      caretColor: 'var(--color-primary-500)',
    },
    '.cm-gutters': {
      backgroundColor: 'transparent',
      color: 'var(--color-surface-400)',
      border: 'none',
    },
    '.cm-activeLine': {
      backgroundColor: 'rgba(127, 127, 127, 0.05)',
    },
    '.cm-activeLineGutter': {
      backgroundColor: 'transparent',
    },
    '&.cm-focused': {
      outline: 'none',
    },
    '&.cm-focused .cm-cursor': {
      borderLeftColor: 'var(--color-primary-500)',
    },
  })

  function buildState(initial: string): EditorState {
    return EditorState.create({
      doc: initial,
      extensions: [
        lineNumbers(),
        history(),
        foldGutter(),
        indentOnInput(),
        bracketMatching(),
        syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
        highlightActiveLine(),
        markdown(),
        keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
        themeCompartment.of(editorTheme),
        // Dispatch every doc-changing transaction back up to the
        // parent's onchange.  Skipping selection-only changes keeps
        // the auto-save debounce from firing on cursor moves.
        EditorView.updateListener.of((u) => {
          if (!u.docChanged) return
          const next = u.state.doc.toString()
          if (next !== value) {
            value = next
            onchange?.(next)
          }
        }),
        // #260: trigger detection for `@` and `/mail`.  The plugin
        // emits a context whenever the cursor sits at the end of a
        // mention token; we hand it to the data resolver below.
        createMentionExtension({
          onContextChange: (ctx) => {
            // Diagnostic logging — remove once verified working.
            console.debug('[notes-mention] context →', ctx)
            mentionCtx = ctx
            resolvePickerItems(ctx)
          },
        }),
      ],
    })
  }

  /** Look up rows for the current mention context.  Contact lookup
   *  is synchronous (filter the in-memory `allContacts`).  Mail
   *  lookup hits `search_emails` (cross-folder FTS5 cache) and
   *  optionally falls back to `search_imap_server` for the active
   *  account's INBOX when the cache returns sparse results. */
  function resolvePickerItems(ctx: MentionContext | null): void {
    // Cancel any in-flight `/mail` debounce — a fresh context (even
    // null, meaning "popup closed") supersedes it.
    if (mailDebounce !== null) {
      clearTimeout(mailDebounce)
      mailDebounce = null
    }
    pickerQueryToken += 1
    if (!ctx) {
      pickerItems = []
      pickerLoading = false
      return
    }
    if (ctx.type === 'contact') {
      pickerItems = filterContacts(ctx.query)
      pickerLoading = false
      return
    }
    // `/mail` — debounce 200 ms (per the issue's note about not
    // spamming the search on every keystroke).
    const myToken = pickerQueryToken
    // Surface a "Searching…" state immediately when there's nothing
    // to fall back to; otherwise keep the previous result list
    // visible so the user doesn't lose context while typing.
    if (pickerItems.length === 0) pickerLoading = true
    mailDebounce = setTimeout(() => {
      mailDebounce = null
      void runMailSearch(ctx.query, myToken)
    }, 200)
  }

  /** Filter the eager-loaded address book by a case-insensitive
   *  substring on display_name + email.  One row per email
   *  (contacts with multiple addresses each get their own row),
   *  capped at 8 to keep the popup tight. */
  function filterContacts(query: string): ContactItem[] {
    const q = query.trim().toLowerCase()
    const out: ContactItem[] = []
    const seen = new Set<string>()
    for (const c of allContacts) {
      for (const e of c.email) {
        const email = e.value
        if (!email) continue
        const key = email.toLowerCase()
        if (seen.has(key)) continue
        const labelMatch =
          !q ||
          c.display_name.toLowerCase().includes(q) ||
          email.toLowerCase().includes(q)
        if (!labelMatch) continue
        seen.add(key)
        out.push({
          kind: 'contact',
          id: email,
          label: c.display_name || email,
          email,
          photoUrl: c.photo_mime ? convertFileSrc(c.id, 'contact-photo') : null,
          hint: c.organization,
        })
        if (out.length >= 8) return out
      }
    }
    return out
  }

  /** Run the cache-first / server-fallback `/mail` search.  The
   *  `token` guards against late responses overwriting fresher
   *  state — every fetch is started against the current
   *  `pickerQueryToken` and bails if a later context bumped it. */
  async function runMailSearch(query: string, token: number): Promise<void> {
    pickerLoading = true
    let hits: SearchHit[] = []
    try {
      // Empty scope / filters = "search everything cached".  The
      // backend command accepts the operator-prefixed grammar
      // (FROM:, SUBJECT:, etc.) so power users get the full
      // syntax for free here too.
      hits = await invoke<SearchHit[]>('search_emails', { query })
    } catch (e) {
      console.warn('NotesMarkdownEditor: search_emails failed', e)
    }
    if (token !== pickerQueryToken) return
    let rows = hits.slice(0, 8).map(hitToMailItem)
    // Server-side fallback (#260): if the local cache returned
    // sparse results and we have an active non-JMAP account, also
    // hit IMAP `UID SEARCH` on its INBOX so recent / unsynced mail
    // is reachable.  Per-account-INBOX scope is a deliberate
    // simplification: cross-folder server search is much slower
    // and the picker has to feel responsive.  Documented limitation.
    if (
      rows.length < 5 &&
      query.trim().length >= 3 &&
      accountId &&
      typeof accountId === 'string'
    ) {
      try {
        const fallback = await invoke<IMAPEnvelope[]>('search_imap_server', {
          accountId,
          folder: 'INBOX',
          query,
          limit: 16,
        })
        if (token !== pickerQueryToken) return
        const seenUids = new Set(rows.map((r) => `${r.accountId}:${r.folder}:${r.uid}`))
        for (const env of fallback) {
          const aid = env.account_id ?? accountId
          const folder = env.folder ?? 'INBOX'
          const key = `${aid}:${folder}:${env.uid}`
          if (seenUids.has(key)) continue
          seenUids.add(key)
          rows.push({
            kind: 'mail',
            id: key,
            accountId: aid,
            folder,
            uid: env.uid,
            subject: env.subject,
            from: env.from,
            date: env.date,
            isRead: env.is_read ?? true,
          })
          if (rows.length >= 12) break
        }
      } catch (e) {
        // JMAP accounts return empty (handled server-side); other
        // failures (offline, auth) shouldn't break the picker.
        console.warn('NotesMarkdownEditor: search_imap_server fallback failed', e)
      }
    }
    if (token !== pickerQueryToken) return
    pickerItems = rows
    pickerLoading = false
  }

  function hitToMailItem(h: SearchHit): MailItem {
    return {
      kind: 'mail',
      id: `${h.accountId}:${h.folder}:${h.uid}`,
      accountId: h.accountId,
      folder: h.folder,
      uid: h.uid,
      subject: h.subject,
      from: h.from,
      date: h.date,
      snippet: h.snippet,
      isRead: h.isRead,
    }
  }

  /** Commit a picked item — replaces the trigger span with a
   *  markdown link.  Contact picks become `[Name](mailto:email)`;
   *  mail picks become `[subject](mail://acc/folder/uid)`. */
  function onPick(item: PickerItem): void {
    if (!view || !mentionCtx) return
    let markdown: string
    if (item.kind === 'contact') {
      const safeLabel = escapeMarkdownLink(item.label)
      markdown = `[${safeLabel}](mailto:${item.email})`
    } else {
      const safeSubject = escapeMarkdownLink(item.subject || '(no subject)')
      const folderPart = encodeURIComponent(item.folder)
      markdown = `[${safeSubject}](mail://${item.accountId}/${folderPart}/${item.uid})`
    }
    insertMention(view, mentionCtx, markdown)
    mentionCtx = null
    pickerItems = []
  }

  /** Defensive escape for the link label.  Markdown link syntax
   *  uses `]` to end the label, so a `]` inside the display name
   *  would truncate it.  Backslash-escape just the closers; the
   *  rest stays readable.  Backtick / `*` etc. are allowed by
   *  GFM inside link text without escaping. */
  function escapeMarkdownLink(s: string): string {
    return s.replace(/\\/g, '\\\\').replace(/\]/g, '\\]')
  }

  /** Editor-level keyboard handler.  Intercepts the picker's keys
   *  *before* CM6 routes them so Enter inserts the mention rather
   *  than a newline, Tab commits rather than indents, etc.  Pure
   *  pass-through when the popup isn't visible. */
  function onEditorKeyDown(e: KeyboardEvent): void {
    if (!mentionCtx) return
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      picker?.selectNext()
      return
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault()
      picker?.selectPrev()
      return
    }
    if (e.key === 'Enter' || e.key === 'Tab') {
      // Only swallow the key when there's something to pick — an
      // empty popup (e.g. mid-search with no cache hits yet)
      // shouldn't trap Enter / Tab.
      if (pickerItems.length === 0) return
      e.preventDefault()
      picker?.pickSelected()
      return
    }
    if (e.key === 'Escape') {
      e.preventDefault()
      mentionCtx = null
      pickerItems = []
    }
  }

  /** Notes-preview link click delegate (#260).  Intercepts
   *  `mail://acc/folder/uid` links and routes them through the
   *  parent's `onopenmail` callback (which composes the account /
   *  folder / message state changes).  All other links — http(s)
   *  to NC files, `mailto:` to external mail clients, etc. — fall
   *  through to the browser's default handling. */
  function onPreviewClick(e: MouseEvent): void {
    const target = (e.target as HTMLElement | null)?.closest('a')
    if (!target) return
    const href = target.getAttribute('href') ?? ''
    if (!href.startsWith('mail://')) return
    e.preventDefault()
    const parsed = parseMailHref(href)
    if (!parsed) return
    onopenmail?.(parsed.accountId, parsed.folder, parsed.uid)
  }

  /** Parse a `mail://<account_id>/<folder>/<uid>` URL into its
   *  pieces.  Returns `null` on any malformed input — folder
   *  segment is `decodeURIComponent`'d so folders with slashes /
   *  spaces (rare but possible) round-trip cleanly. */
  function parseMailHref(
    href: string,
  ): { accountId: string; folder: string; uid: number } | null {
    if (!href.startsWith('mail://')) return null
    const rest = href.slice('mail://'.length)
    // `accountId / folder / uid` — split into at most three parts
    // so a folder name containing `/` survives the round-trip.
    const firstSlash = rest.indexOf('/')
    if (firstSlash < 0) return null
    const accountId = rest.slice(0, firstSlash)
    const lastSlash = rest.lastIndexOf('/')
    if (lastSlash <= firstSlash) return null
    const folderRaw = rest.slice(firstSlash + 1, lastSlash)
    const uidStr = rest.slice(lastSlash + 1)
    const uid = Number(uidStr)
    if (!accountId || !folderRaw || !Number.isFinite(uid) || uid <= 0) {
      return null
    }
    return {
      accountId,
      folder: decodeURIComponent(folderRaw),
      uid,
    }
  }

  onMount(() => {
    if (!host) return
    view = new EditorView({
      state: buildState(value),
      parent: host,
    })
  })

  onDestroy(() => {
    view?.destroy()
    view = undefined
  })

  // External `value` changes (loading a different note) — push
  // them into the editor only when they don't match what the
  // editor already has, otherwise we'd echo every keystroke.
  $effect(() => {
    if (!view) return
    const current = view.state.doc.toString()
    if (current !== value) {
      view.dispatch({
        changes: { from: 0, to: current.length, insert: value },
      })
    }
  })

  // Markdown → HTML for the preview pane.  Re-runs reactively on
  // value change.  `marked` returns Promise in some configs;
  // `marked.parse()` synchronously is what we want here.
  const previewHtml = $derived.by(() => {
    if (!showPreview) return ''
    try {
      return marked.parse(value || '', {
        breaks: true,
        gfm: true,
      }) as string
    } catch {
      return '<p><em>Could not render preview.</em></p>'
    }
  })
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="markdown-editor-shell">
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="markdown-editor-source"
    class:has-preview={showPreview}
    onkeydowncapture={onEditorKeyDown}
    role="presentation"
  >
    <div bind:this={host} class="markdown-editor-host" data-placeholder={placeholder}></div>
  </div>
  {#if showPreview}
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
      class="markdown-editor-preview prose prose-sm dark:prose-invert"
      onclick={onPreviewClick}
    >
      {@html previewHtml}
    </div>
  {/if}
</div>

<!-- Mention picker popup (#260).  Mounted at the editor level so
     `position: fixed` positioning isn't clipped by the Notes
     view's overflow boundaries.  Only renders when the CM6 plugin
     reports an active mention context. -->
<NotesMentionPicker
  bind:this={picker}
  items={pickerItems}
  visible={mentionCtx !== null}
  loading={pickerLoading}
  anchor={mentionCtx
    ? mentionCtx.coords
    : { left: 0, top: 0, bottom: 0 }}
  onpick={onPick}
  onclose={() => {
    mentionCtx = null
    pickerItems = []
  }}
/>

<style>
  :global(.markdown-editor-shell) {
    flex: 1;
    min-height: 0;
    display: flex;
    overflow: hidden;
  }
  :global(.markdown-editor-source) {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  :global(.markdown-editor-source.has-preview) {
    border-right: 1px solid var(--color-surface-200);
  }
  :global([data-mode='dark'] .markdown-editor-source.has-preview) {
    border-right-color: var(--color-surface-700);
  }
  :global(.markdown-editor-host) {
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }
  :global(.markdown-editor-host .cm-editor) {
    height: 100%;
  }
  :global(.markdown-editor-preview) {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 1rem 1.25rem;
    max-width: none;
  }
  :global(.markdown-editor-preview h1),
  :global(.markdown-editor-preview h2),
  :global(.markdown-editor-preview h3) {
    margin-top: 0.875rem;
    margin-bottom: 0.5rem;
  }
  :global(.markdown-editor-preview pre) {
    background: var(--color-surface-100);
    padding: 0.75rem;
    border-radius: 0.375rem;
    overflow-x: auto;
  }
  :global([data-mode='dark'] .markdown-editor-preview pre) {
    background: var(--color-surface-800);
  }
  :global(.markdown-editor-preview code) {
    font-family: ui-monospace, SFMono-Regular, monospace;
    font-size: 0.85em;
  }
</style>

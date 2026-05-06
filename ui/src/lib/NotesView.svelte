<script lang="ts">
  /**
   * NotesView — sidebar-routed Nextcloud Notes browser + editor.
   *
   * Three-column layout (#138):
   *   Sidebar  → folder tree built from `/`-separated category
   *              paths, plus virtuals (All / Favorites /
   *              Uncategorized).  Same WebDAV-style nesting that NC's
   *              own web UI renders, so a Joplin-via-WebDAV setup
   *              (`Joplin/Notebook A/...`) lights up automatically.
   *   List     → notes filtered by the selected sidebar entry,
   *              modified-desc.
   *   Editor   → title + textarea + per-note category picker.
   *
   * # Caching
   *
   * Reads come from the local cache (`list_nextcloud_notes`) so the
   * list paints instantly and works offline; a `sync_nextcloud_notes`
   * round-trip runs in the background after the cache load and on a
   * 120 s timer thereafter.  Writes go through `*_nextcloud_note`,
   * which hits the server first (so a 412 etag mismatch surfaces
   * before we touch local state) and then upserts the cache.
   *
   * # "+ Add folder"
   *
   * NC categories don't exist independently of notes — a category
   * "exists" only as long as some note carries it.  So a freshly
   * created folder lives in `localStorage` per account until the
   * user actually creates a note in it; that's enough to make the
   * UX feel persistent without leaving stub notes lying around.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { onDestroy, onMount } from 'svelte'
  import { formatError } from './errors'
  import type { ComposeInitial } from './Compose.svelte'

  interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
  }

  /** Mirrors `nimbus_core::models::Note`. */
  interface Note {
    id: number
    nextcloud_account_id: string
    etag: string
    modified: number
    title: string
    category: string
    content: string
    favorite: boolean
  }

  /** Sidebar selection — either a virtual bucket or a real
   *  category path.  Kept as a discriminated union so the filter
   *  logic doesn't have to special-case strings. */
  type Selection =
    | { kind: 'all' }
    | { kind: 'favorites' }
    | { kind: 'uncategorized' }
    | { kind: 'category'; path: string }

  interface Props {
    onclose: () => void
    /** Open Compose with the given prefill (used for "Send as email"). */
    oncompose: (initial: ComposeInitial) => void
  }
  const { onclose, oncompose }: Props = $props()

  // ── State ───────────────────────────────────────────────────
  let accounts = $state<NextcloudAccount[]>([])
  let accountId = $state('')
  let notes = $state<Note[]>([])
  let loading = $state(false)
  let syncing = $state(false)
  let error = $state('')

  /** Folders the user has explicitly created via "+ Add folder"
   *  but hasn't yet populated.  Persisted per-account in
   *  localStorage so they survive reloads.  Empties out as the
   *  user creates notes inside them (the path then exists on the
   *  server). */
  let pendingFolders = $state<Set<string>>(new Set())
  /** Set of folder paths the user has explicitly collapsed in the
   *  tree.  Default-open so the sidebar isn't a wall of carets. */
  let collapsedFolders = $state<Set<string>>(new Set())
  let selection = $state<Selection>({ kind: 'all' })

  /** Currently selected note id, or `null` for the empty-state pane. */
  let selectedId = $state<number | null>(null)
  /** Working copy of the selected note's editable fields. */
  let draftTitle = $state('')
  let draftContent = $state('')
  let draftCategory = $state('')
  /** etag we last loaded the open note at — sent back on update so
   *  the server can reject (412) if a concurrent edit landed. */
  let draftEtag = $state('')
  let saveStatus = $state<'' | 'saving' | 'saved' | 'error'>('')

  const REFRESH_INTERVAL_MS = 120_000
  let pollTimer: number | null = null

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
        loadPendingFolders()
        await loadFromCache()
        startPolling()
        void syncNow({ silent: true })
      }
    } catch (e) {
      error = formatError(e) || 'Failed to load Nextcloud accounts'
    }
  }

  async function selectAccount(id: string) {
    accountId = id
    notes = []
    selectedId = null
    selection = { kind: 'all' }
    loadPendingFolders()
    await loadFromCache()
    startPolling()
    void syncNow({ silent: true })
  }

  function startPolling() {
    if (pollTimer !== null) window.clearInterval(pollTimer)
    pollTimer = window.setInterval(() => {
      void syncNow({ silent: true })
    }, REFRESH_INTERVAL_MS)
  }

  /** Cache read — instant, offline-safe.  Always paints something
   *  before the network has a chance to come back. */
  async function loadFromCache() {
    if (!accountId) return
    loading = true
    error = ''
    try {
      const list = await invoke<Note[]>('list_nextcloud_notes', { ncId: accountId })
      notes = list
      if (selectedId != null && !list.some((n) => n.id === selectedId)) {
        selectedId = null
      }
    } catch (e) {
      error = formatError(e) || 'Failed to load notes'
    } finally {
      loading = false
    }
  }

  /** Network refresh — pulls every note from NC, applies the delta
   *  to the cache, then refreshes our in-memory list.  `silent`
   *  hides the spinner so the polling timer doesn't flicker the
   *  UI for a no-op delta. */
  async function syncNow(opts: { silent?: boolean } = {}) {
    if (!accountId) return
    if (!opts.silent) syncing = true
    try {
      const list = await invoke<Note[]>('sync_nextcloud_notes', { ncId: accountId })
      notes = list
      // Drop any pending folder that the server now reports as
      // having at least one note in it — the folder is no longer
      // "pending" once it's a real category.
      if (pendingFolders.size > 0) {
        const live = new Set(list.map((n) => n.category).filter((c) => c.length > 0))
        let changed = false
        const next = new Set(pendingFolders)
        for (const p of pendingFolders) {
          // A category exists if any prefix of its path is in `live`,
          // because nested paths imply their parent folders.
          for (const cat of live) {
            if (cat === p || cat.startsWith(p + '/')) {
              next.delete(p)
              changed = true
              break
            }
          }
        }
        if (changed) {
          pendingFolders = next
          savePendingFolders()
        }
      }
      if (selectedId != null && !list.some((n) => n.id === selectedId)) {
        selectedId = null
      }
    } catch (e) {
      if (!opts.silent) error = formatError(e) || 'Failed to sync notes'
    } finally {
      if (!opts.silent) syncing = false
    }
  }

  // ── Folder tree ─────────────────────────────────────────────
  /** Tree node built from category paths + pending folders. */
  interface FolderNode {
    name: string // last path segment
    path: string // full slash-joined path
    children: FolderNode[]
    /** Number of notes whose category is exactly this path. */
    directCount: number
    /** Number of notes whose category starts with this path
     *  (inclusive of `directCount`).  Drives the badge in the
     *  sidebar so collapsing a tree still shows total content. */
    descendantCount: number
  }

  const folderTree = $derived.by((): FolderNode[] => {
    const root: FolderNode = {
      name: '',
      path: '',
      children: [],
      directCount: 0,
      descendantCount: 0,
    }
    function ensure(parent: FolderNode, segments: string[], fullPath: string) {
      if (segments.length === 0) return parent
      const head = segments[0]
      let child = parent.children.find((c) => c.name === head)
      if (!child) {
        child = {
          name: head,
          path: parent.path ? `${parent.path}/${head}` : head,
          children: [],
          directCount: 0,
          descendantCount: 0,
        }
        parent.children.push(child)
      }
      return ensure(child, segments.slice(1), fullPath)
    }
    // Real categories from notes.
    for (const n of notes) {
      const cat = n.category.trim()
      if (!cat) continue
      const segments = cat.split('/').filter((s) => s.length > 0)
      const leaf = ensure(root, segments, cat)
      leaf.directCount += 1
      // Bump descendantCount up the chain.
      let cursor: FolderNode | null = leaf
      while (cursor && cursor.path !== '') {
        cursor.descendantCount += 1
        cursor = findParent(root, cursor.path)
      }
    }
    // Pending (user-created, not yet populated) folders.
    for (const p of pendingFolders) {
      const segments = p.split('/').filter((s) => s.length > 0)
      ensure(root, segments, p)
    }
    sortTree(root)
    return root.children
  })

  function findParent(root: FolderNode, path: string): FolderNode | null {
    const segments = path.split('/').filter((s) => s.length > 0)
    if (segments.length <= 1) return null
    const parentPath = segments.slice(0, -1).join('/')
    return findByPath(root, parentPath)
  }
  function findByPath(node: FolderNode, path: string): FolderNode | null {
    if (node.path === path) return node
    for (const c of node.children) {
      const hit = findByPath(c, path)
      if (hit) return hit
    }
    return null
  }
  function sortTree(node: FolderNode) {
    node.children.sort((a, b) => a.name.localeCompare(b.name))
    for (const c of node.children) sortTree(c)
  }

  const totalNotes = $derived(notes.length)
  const favoriteCount = $derived(notes.filter((n) => n.favorite).length)
  const uncategorizedCount = $derived(
    notes.filter((n) => !n.category.trim()).length,
  )

  /** Notes shown in the middle pane based on the sidebar
   *  selection.  Always sorted by `modified` desc — the cache
   *  already returns rows that way but sorting here keeps the
   *  contract local. */
  const filteredNotes = $derived.by((): Note[] => {
    let list: Note[]
    switch (selection.kind) {
      case 'favorites':
        list = notes.filter((n) => n.favorite)
        break
      case 'uncategorized':
        list = notes.filter((n) => !n.category.trim())
        break
      case 'category': {
        const prefix = selection.path
        list = notes.filter((n) => {
          const c = n.category
          return c === prefix || c.startsWith(prefix + '/')
        })
        break
      }
      case 'all':
      default:
        list = [...notes]
    }
    return list.sort((a, b) => b.modified - a.modified)
  })

  // ── Pending folder persistence ──────────────────────────────
  function pendingFoldersKey(): string {
    return `nimbus.notes.pendingFolders.${accountId}`
  }
  function loadPendingFolders() {
    try {
      const raw = window.localStorage.getItem(pendingFoldersKey())
      pendingFolders = raw ? new Set<string>(JSON.parse(raw)) : new Set()
    } catch {
      pendingFolders = new Set()
    }
  }
  function savePendingFolders() {
    try {
      window.localStorage.setItem(
        pendingFoldersKey(),
        JSON.stringify([...pendingFolders]),
      )
    } catch {
      // localStorage full / disabled — pending folders just won't
      // persist this session.  Not fatal.
    }
  }

  function addFolder() {
    const raw = window.prompt(
      "New folder name (use '/' for nested folders, e.g. 'Work/Project A')",
    )
    if (raw == null) return
    const path = raw
      .split('/')
      .map((s) => s.trim())
      .filter((s) => s.length > 0)
      .join('/')
    if (!path) return
    pendingFolders = new Set(pendingFolders).add(path)
    savePendingFolders()
    selection = { kind: 'category', path }
  }

  // ── Note CRUD wired to the new write-through commands ───────
  function openNote(note: Note) {
    selectedId = note.id
    draftTitle = note.title
    draftContent = note.content
    draftCategory = note.category
    draftEtag = note.etag
    saveStatus = ''
    if (saveTimer !== null) {
      clearTimeout(saveTimer)
      saveTimer = null
    }
  }

  async function newNote() {
    if (!accountId) return
    // If the sidebar is sitting on a category, drop the new note
    // there — matches how every other folder-aware app handles
    // "+ New" while a folder is selected.
    const initialCategory =
      selection.kind === 'category' ? selection.path : ''
    try {
      const created = await invoke<Note>('create_nextcloud_note', {
        ncId: accountId,
        title: '',
        content: '',
        category: initialCategory,
      })
      notes = [created, ...notes]
      // If the new note's category was a pending folder, it's no
      // longer pending — drop it.
      if (initialCategory && pendingFolders.has(initialCategory)) {
        const next = new Set(pendingFolders)
        next.delete(initialCategory)
        pendingFolders = next
        savePendingFolders()
      }
      openNote(created)
    } catch (e) {
      error = formatError(e) || 'Failed to create note'
    }
  }

  async function deleteSelected() {
    if (!accountId || selectedId == null) return
    const note = notes.find((n) => n.id === selectedId)
    if (!note) return
    const label = note.title.trim() || 'this note'
    if (!confirm(`Delete ${label}? This cannot be undone.`)) return
    try {
      await invoke('delete_nextcloud_note', { ncId: accountId, noteId: note.id })
      notes = notes.filter((n) => n.id !== note.id)
      selectedId = null
      saveStatus = ''
    } catch (e) {
      error = formatError(e) || 'Failed to delete note'
    }
  }

  async function toggleFavorite() {
    if (!accountId || selectedId == null) return
    const note = notes.find((n) => n.id === selectedId)
    if (!note) return
    try {
      const updated = await invoke<Note>('update_nextcloud_note', {
        ncId: accountId,
        noteId: note.id,
        etag: note.etag,
        title: null,
        content: null,
        category: null,
        favorite: !note.favorite,
      })
      const rest = notes.filter((n) => n.id !== updated.id)
      notes = [updated, ...rest].sort((a, b) => b.modified - a.modified)
      if (selectedId === updated.id) draftEtag = updated.etag
    } catch (e) {
      error = formatError(e) || 'Failed to update favorite'
    }
  }

  // ── Auto-save (title + content, debounced) ──────────────────
  let saveTimer: ReturnType<typeof setTimeout> | null = null

  function scheduleSave() {
    if (selectedId == null || !accountId) return
    saveStatus = 'saving'
    if (saveTimer !== null) clearTimeout(saveTimer)
    saveTimer = setTimeout(saveNow, 800)
  }

  async function saveNow() {
    if (selectedId == null || !accountId) return
    const id = selectedId
    const titleNow = draftTitle
    const contentNow = draftContent
    const categoryNow = draftCategory
    try {
      const updated = await invoke<Note>('update_nextcloud_note', {
        ncId: accountId,
        noteId: id,
        etag: draftEtag,
        title: titleNow,
        content: contentNow,
        category: categoryNow,
        favorite: null,
      })
      const rest = notes.filter((n) => n.id !== updated.id)
      notes = [updated, ...rest].sort((a, b) => b.modified - a.modified)
      // If the category we just saved was a pending folder, it's
      // now real — drop it from the pending set.
      if (categoryNow && pendingFolders.has(categoryNow)) {
        const next = new Set(pendingFolders)
        next.delete(categoryNow)
        pendingFolders = next
        savePendingFolders()
      }
      if (id === selectedId) draftEtag = updated.etag
      saveStatus = 'saved'
      setTimeout(() => {
        if (saveStatus === 'saved') saveStatus = ''
      }, 1500)
    } catch (e) {
      console.warn('save note failed', e)
      saveStatus = 'error'
    }
  }

  /** "Send as email" — opens Compose with the note as the seed. */
  function sendAsEmail() {
    const note = notes.find((n) => n.id === selectedId)
    if (!note) return
    oncompose({
      subject: note.title || '(untitled note)',
      body: note.content,
    })
  }

  function fmtDate(epochSecs: number): string {
    if (!epochSecs) return ''
    const d = new Date(epochSecs * 1000)
    const now = new Date()
    const sameDay = d.toDateString() === now.toDateString()
    if (sameDay) {
      return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    }
    return d.toLocaleDateString([], { month: 'short', day: 'numeric' })
  }

  function preview(note: Note): string {
    const lines = note.content.split('\n').map((l) => l.trim()).filter(Boolean)
    const title = note.title.trim()
    const body = title && lines[0] === title ? lines.slice(1) : lines
    return body[0] ?? ''
  }

  function selectionMatches(s: Selection, candidate: Selection): boolean {
    if (s.kind !== candidate.kind) return false
    if (s.kind === 'category' && candidate.kind === 'category') {
      return s.path === candidate.path
    }
    return true
  }

  function toggleCollapsed(path: string) {
    const next = new Set(collapsedFolders)
    if (next.has(path)) next.delete(path)
    else next.add(path)
    collapsedFolders = next
  }
</script>

<div class="h-full flex flex-col bg-surface-50 dark:bg-surface-900">
  <!-- Header -->
  <header class="px-4 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center gap-3">
    <h1 class="text-lg font-semibold flex-1">Notes</h1>

    {#if accounts.length > 1}
      <select
        class="select text-sm py-1 px-2 rounded-md"
        value={accountId}
        onchange={(e) => selectAccount((e.currentTarget as HTMLSelectElement).value)}
      >
        {#each accounts as a (a.id)}
          <option value={a.id}>{a.display_name || a.username}</option>
        {/each}
      </select>
    {/if}

    <button
      class="btn btn-sm preset-outlined-surface-500"
      onclick={() => syncNow()}
      disabled={syncing || !accountId}
      title="Refresh from Nextcloud"
    >
      {syncing ? '⟳' : '↻'}
    </button>
    <button
      class="btn btn-sm preset-filled-primary-500"
      onclick={newNote}
      disabled={!accountId}
    >
      + New note
    </button>
    <button class="btn btn-sm preset-outlined-surface-500" onclick={onclose}>
      Back
    </button>
  </header>

  <!-- Body: sidebar | list | editor -->
  <div class="flex-1 min-h-0 flex">
    {#if accounts.length === 0 && !loading}
      <div class="flex-1 flex items-center justify-center text-sm text-surface-500 p-8 text-center">
        Connect a Nextcloud account first (Settings → Nextcloud) to use Notes.
      </div>
    {:else}
      <!-- Sidebar: virtuals + folder tree + add-folder -->
      <aside class="w-56 shrink-0 border-r border-surface-200 dark:border-surface-700 overflow-y-auto py-2 text-sm">
        <!-- Virtuals -->
        <button
          class="notes-side-row {selectionMatches(selection, { kind: 'all' }) ? 'is-active' : ''}"
          onclick={() => (selection = { kind: 'all' })}
        >
          <span class="notes-side-icon">📋</span>
          <span class="flex-1 truncate text-left">All notes</span>
          <span class="notes-side-count">{totalNotes}</span>
        </button>
        <button
          class="notes-side-row {selectionMatches(selection, { kind: 'favorites' }) ? 'is-active' : ''}"
          onclick={() => (selection = { kind: 'favorites' })}
        >
          <span class="notes-side-icon">⭐</span>
          <span class="flex-1 truncate text-left">Favorites</span>
          <span class="notes-side-count">{favoriteCount}</span>
        </button>
        <button
          class="notes-side-row {selectionMatches(selection, { kind: 'uncategorized' }) ? 'is-active' : ''}"
          onclick={() => (selection = { kind: 'uncategorized' })}
        >
          <span class="notes-side-icon">📄</span>
          <span class="flex-1 truncate text-left">Uncategorized</span>
          <span class="notes-side-count">{uncategorizedCount}</span>
        </button>

        <div class="my-2 border-t border-surface-200 dark:border-surface-700"></div>

        <!-- Folder tree.  Recursive render via a snippet so a
             5-level deep Joplin layout looks the same as the
             flat NC categories. -->
        {#snippet folder(node: FolderNode, depth: number)}
          {@const isCollapsed = collapsedFolders.has(node.path)}
          {@const hasChildren = node.children.length > 0}
          {@const isSelected = selectionMatches(selection, { kind: 'category', path: node.path })}
          <div class="notes-side-row {isSelected ? 'is-active' : ''}" style="padding-left: {0.5 + depth * 0.75}rem">
            {#if hasChildren}
              <button
                type="button"
                class="notes-side-caret"
                onclick={(e) => {
                  e.stopPropagation()
                  toggleCollapsed(node.path)
                }}
                aria-label={isCollapsed ? 'Expand folder' : 'Collapse folder'}
              >
                {isCollapsed ? '▸' : '▾'}
              </button>
            {:else}
              <span class="notes-side-caret-spacer"></span>
            {/if}
            <button
              type="button"
              class="flex-1 flex items-center gap-1.5 truncate text-left"
              onclick={() => (selection = { kind: 'category', path: node.path })}
            >
              <span class="notes-side-icon">📁</span>
              <span class="truncate">{node.name}</span>
            </button>
            {#if node.descendantCount > 0}
              <span class="notes-side-count">{node.descendantCount}</span>
            {/if}
          </div>
          {#if hasChildren && !isCollapsed}
            {#each node.children as child (child.path)}
              {@render folder(child, depth + 1)}
            {/each}
          {/if}
        {/snippet}

        {#each folderTree as root (root.path)}
          {@render folder(root, 0)}
        {/each}

        <button
          type="button"
          class="notes-side-add"
          onclick={addFolder}
          disabled={!accountId}
        >+ Add folder</button>
      </aside>

      <!-- List pane -->
      <div class="w-72 shrink-0 border-r border-surface-200 dark:border-surface-700 overflow-y-auto">
        {#if loading && notes.length === 0}
          <div class="p-6 text-center text-sm text-surface-500">Loading…</div>
        {:else if error && notes.length === 0}
          <div class="p-4 text-sm text-red-500">{error}</div>
        {:else if filteredNotes.length === 0}
          <div class="p-6 text-center text-sm text-surface-500">
            {#if selection.kind === 'all'}
              No notes yet. Click <strong>+ New note</strong> to create one.
            {:else}
              No notes in this folder.
            {/if}
          </div>
        {:else}
          {#each filteredNotes as n (n.id)}
            <button
              class="w-full text-left px-4 py-3 border-b border-surface-100 dark:border-surface-800 transition-colors
                {selectedId === n.id
                  ? 'bg-primary-500/10'
                  : 'hover:bg-surface-100 dark:hover:bg-surface-800'}"
              onclick={() => openNote(n)}
            >
              <div class="flex items-center justify-between mb-1 gap-2">
                <span class="text-sm font-medium truncate flex-1">
                  {#if n.favorite}<span class="text-warning-500 mr-1">★</span>{/if}
                  {n.title || '(untitled)'}
                </span>
                <span class="text-xs text-surface-500 shrink-0">{fmtDate(n.modified)}</span>
              </div>
              {#if preview(n)}
                <p class="text-xs text-surface-500 truncate">{preview(n)}</p>
              {/if}
              {#if n.category}
                <p class="text-[10px] text-surface-400 mt-1 truncate">📁 {n.category}</p>
              {/if}
            </button>
          {/each}
        {/if}
      </div>

      <!-- Editor pane -->
      <div class="flex-1 min-w-0 flex flex-col">
        {#if selectedId == null}
          <div class="flex-1 flex items-center justify-center text-sm text-surface-500">
            Select a note from the list, or create a new one.
          </div>
        {:else}
          {@const open = notes.find((n) => n.id === selectedId)}
          {#if open}
            <div class="px-5 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
              <input
                class="input flex-1 text-base font-semibold px-3 py-2 rounded-md"
                placeholder="Title (optional — derived from the first line if empty)"
                bind:value={draftTitle}
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
                class="btn btn-sm preset-outlined-surface-500"
                onclick={toggleFavorite}
                title={open.favorite ? 'Unstar' : 'Star'}
              >{open.favorite ? '★' : '☆'}</button>
              <button
                class="btn btn-sm preset-outlined-primary-500"
                onclick={sendAsEmail}
                title="Open Compose with this note as the message body"
              >✉ Send as email</button>
              <button
                class="btn btn-sm preset-outlined-error-500"
                onclick={deleteSelected}
              >Delete</button>
            </div>

            <!-- Category picker — small unobtrusive row above the
                 textarea so the user can move the note between
                 folders without leaving the editor. -->
            <div class="px-5 py-2 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2 text-xs">
              <span class="text-surface-500">Folder:</span>
              <input
                class="input flex-1 text-xs px-2 py-1 rounded-md"
                placeholder="(none) — type a folder path, e.g. Work/Project A"
                bind:value={draftCategory}
                oninput={scheduleSave}
                list="notes-folder-suggestions"
              />
              <datalist id="notes-folder-suggestions">
                {#each Array.from(new Set(notes.map((n) => n.category).filter(Boolean))) as cat (cat)}
                  <option value={cat}></option>
                {/each}
                {#each pendingFolders as p (p)}
                  <option value={p}></option>
                {/each}
              </datalist>
            </div>

            <textarea
              class="flex-1 resize-none p-5 bg-surface-50 dark:bg-surface-900 outline-none font-mono text-sm leading-relaxed"
              placeholder="Start writing — markdown is preserved on the server."
              bind:value={draftContent}
              oninput={scheduleSave}
            ></textarea>
          {/if}
        {/if}
      </div>
    {/if}
  </div>
</div>

<style>
  /* Sidebar row affordance — flat, hover-tinted, primary-flagged
     on the active selection.  No box around individual rows; the
     sidebar's bordered column already does that visually. */
  :global(.notes-side-row) {
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
  :global(.notes-side-row:hover) {
    background: var(--color-surface-100);
  }
  :global([data-mode='dark'] .notes-side-row:hover) {
    background: var(--color-surface-800);
  }
  :global(.notes-side-row.is-active) {
    background: rgba(var(--color-primary-500) r g b / 0.15);
    color: var(--color-primary-600);
    font-weight: 500;
  }
  :global([data-mode='dark'] .notes-side-row.is-active) {
    color: var(--color-primary-300);
  }
  :global(.notes-side-icon) {
    flex-shrink: 0;
    width: 1.125rem;
    text-align: center;
  }
  :global(.notes-side-count) {
    flex-shrink: 0;
    font-size: 0.6875rem;
    color: var(--color-surface-500);
    padding-left: 0.25rem;
  }
  :global(.notes-side-caret) {
    width: 1rem;
    flex-shrink: 0;
    color: var(--color-surface-500);
    background: transparent;
    border: none;
    cursor: pointer;
    padding: 0;
    font-size: 0.625rem;
  }
  :global(.notes-side-caret-spacer) {
    width: 1rem;
    flex-shrink: 0;
  }
  :global(.notes-side-add) {
    width: calc(100% - 1rem);
    margin: 0.5rem 0.5rem 0;
    padding: 0.375rem 0.5rem;
    border-radius: 0.375rem;
    background: transparent;
    color: var(--color-primary-500);
    border: 1px dashed var(--color-surface-300);
    cursor: pointer;
    font-size: 0.8125rem;
  }
  :global(.notes-side-add:hover) {
    background: rgba(var(--color-primary-500) r g b / 0.08);
  }
  :global([data-mode='dark'] .notes-side-add) {
    border-color: var(--color-surface-700);
  }
  :global(.notes-side-add:disabled) {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>

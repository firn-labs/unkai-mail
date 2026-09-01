<script lang="ts">
  /**
   * NotesView — sidebar-routed Nextcloud Notes browser + editor.
   *
   * Stacked view header (#522/#525): title above the icon-only
   * New note + Sync actions, docked left per the integration-view
   * shell.  Below it, a three-column layout (#138):
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

  import * as api from './api'
  import { anchorRect, clampToViewport, cursorAnchor } from './coords'
  import { isNextcloudSource } from './ncSources'
  import { onDestroy, onMount } from 'svelte'
  import { formatError } from './errors'
  import { m } from '../paraglide/messages'
  import type { ComposeInitial } from './Compose.svelte'
  import Icon from './Icon.svelte'
  import SearchInput from './SearchInput.svelte'
  import NotesMarkdownEditor from './NotesMarkdownEditor.svelte'
  import { resizableSidebar } from './resizableSidebar'

  interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
  }

  /** Mirrors `unkai_core::models::Note`. */
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
    /** Open Compose with the given prefill (used for "Send as email"). */
    oncompose: (initial: ComposeInitial) => void
    /** Active *mail* account id (#260).  Distinct from the Notes
     *  view's own `accountId`, which is a Nextcloud account.  The
     *  editor's `/mail` picker uses this to scope the IMAP-server
     *  fallback to whichever inbox the user is currently working
     *  with.  Null when no mail account is configured / active. */
    mailAccountId?: string | null
    /** `mail://acc/folder/uid` link handler (#260).  Called when
     *  the user clicks an in-Note Unkai mail reference in the
     *  preview pane; the parent composes the corresponding view /
     *  account / folder / message state changes. */
    onopenmail?: (accountId: string, folder: string, uid: number) => void
  }
  const {
    oncompose,
    mailAccountId = null,
    onopenmail,
  }: Props = $props()

  // ── State ───────────────────────────────────────────────────
  let accounts = $state<NextcloudAccount[]>([])
  let accountId = $state('')
  let notes = $state<Note[]>([])
  let loading = $state(false)
  let error = $state('')
  /** True while a header-triggered sync round-trip is in flight —
   *  drives the sync button's `loading` icon swap (CLAUDE.md
   *  header-action idiom).  The background polling timer stays
   *  silent and never touches this flag. */
  let syncing = $state(false)

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

  /** Inline "+ Add folder" draft state.  When `creatingFolder` is
   *  true the sidebar renders an extra row at the end of the
   *  folder list with an `<input>` instead of a label — the user
   *  types directly into the row, Enter / blur commits, Escape
   *  cancels.  Matches the rename-style convention from
   *  CLAUDE.md (#UI conventions → "Inline edits over modals"). */
  let creatingFolder = $state(false)
  let newFolderName = $state('')
  let newFolderInput: HTMLInputElement | undefined = $state()

  /** Per-folder action menu state — populated by both the
   *  three-dot trigger and the right-click handler so the two
   *  surfaces share one menu component (CLAUDE.md #UI
   *  conventions). */
  let folderMenu = $state<{ path: string; x: number; y: number } | null>(null)
  /** Confirmation modal for "Remove folder X (N notes will be
   *  uncategorized)".  Pending-only folders skip this and just
   *  drop out of localStorage; only server-real folders surface
   *  it because that's the destructive case. */
  let folderDeleteConfirm = $state<{
    path: string
    affectedCount: number
  } | null>(null)
  /** Disables the menu items while a remove-folder request is
   *  in flight to stop double-clicks from spawning parallel
   *  PUTs that race each other on the etag. */
  let folderOpBusy = $state(false)

  /** Open the centered "Move note to folder" modal.  Mirrors the
   *  shape of `MoveFolderPicker.svelte` from the mail UI — same
   *  backdrop / panel / filter input — adapted for notes (folder
   *  paths instead of IMAP folder names). */
  let movingNote = $state<Note | null>(null)
  /** Filter input inside the move modal.  Kept here so it doesn't
   *  reset every time the modal re-renders. */
  let moveFilter = $state('')

  /** DnD: which folder path is currently the drop target.  Drives
   *  a primary-tinted ring on the hovered sidebar row, same idiom
   *  as `Sidebar.svelte`'s `dragOverFolder` for mail.  `''`
   *  represents the Uncategorized virtual. */
  let dragOverFolder = $state<string | null>(null)
  /** MIME type for our private drag payload — kept distinct from
   *  the mail one so a stray drop between views never tries to
   *  re-categorise as if it were a mail UID. */
  const NOTE_DND_MIME = 'application/x-unkai-note'

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
  /** Show the markdown preview pane alongside the editor.  Off by
   *  default — the source-only view is what most users want most
   *  of the time, the toggle is for double-checking rendering. */
  let showPreview = $state(false)

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
      // Nextcloud-app feature — skip generic-DAV / local sources (#413).
      const list = (
        await api.nextcloud.getNextcloudAccounts()
      ).filter(isNextcloudSource)
      accounts = list
      if (list.length >= 1 && !accountId) {
        accountId = list[0].id
        loadPendingFolders()
        await loadFromCache()
        startPolling()
        void syncNow()
      }
    } catch (e) {
      error = formatError(e) || m.notes_view_err_load_accounts()
    }
  }

  /** Header sync button — the same round-trip the timer runs, but
   *  with the `loading` icon swap so the user sees it working. */
  async function manualSync() {
    if (syncing) return
    syncing = true
    try {
      await syncNow()
    } finally {
      syncing = false
    }
  }

  async function selectAccount(id: string) {
    accountId = id
    notes = []
    selectedId = null
    selection = { kind: 'all' }
    searchQuery = ''
    loadPendingFolders()
    await loadFromCache()
    startPolling()
    void syncNow()
  }

  function startPolling() {
    if (pollTimer !== null) window.clearInterval(pollTimer)
    pollTimer = window.setInterval(() => {
      void syncNow()
    }, REFRESH_INTERVAL_MS)
  }

  /** Cache read — instant, offline-safe.  Always paints something
   *  before the network has a chance to come back. */
  async function loadFromCache() {
    if (!accountId) return
    loading = true
    error = ''
    try {
      const list = await api.notes.listNextcloudNotes({ ncId: accountId })
      notes = list
      if (selectedId != null && !list.some((n) => n.id === selectedId)) {
        selectedId = null
      }
    } catch (e) {
      error = formatError(e) || m.notes_view_err_load()
    } finally {
      loading = false
    }
  }

  /** Network refresh — pulls every note from NC, applies the
   *  delta to the cache, then refreshes our in-memory list.
   *  Runs from the boot path, the 120 s polling timer, and the
   *  header sync button (via `manualSync`, which adds the
   *  loading-icon swap).  Silent on failure unless the cache is
   *  empty — then the `error` string explains the blank list. */
  async function syncNow() {
    if (!accountId) return
    try {
      const list = await api.notes.syncNextcloudNotes({ ncId: accountId })
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
      // Surface only when the user has nothing else to look at —
      // background polling errors (we have a populated cache) are
      // noise that don't help the user.
      if (notes.length === 0) {
        error = formatError(e) || m.notes_view_err_sync()
      } else {
        console.warn('background notes sync failed', e)
      }
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

  /** Search query for the list pane.  Matches against title /
   *  content / category, case-insensitive.  Independent of the
   *  sidebar selection so a user can search "within Joplin" or
   *  "within all notes" without switching folders.  Clears on
   *  account switch (we set it explicitly in `selectAccount`). */
  let searchQuery = $state('')

  /** Notes shown in the middle pane based on the sidebar
   *  selection AND the search query.  Always sorted by
   *  `modified` desc — the cache already returns rows that way
   *  but sorting here keeps the contract local. */
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
    const q = searchQuery.trim().toLowerCase()
    if (q) {
      list = list.filter(
        (n) =>
          n.title.toLowerCase().includes(q) ||
          n.content.toLowerCase().includes(q) ||
          n.category.toLowerCase().includes(q),
      )
    }
    return list.sort((a, b) => b.modified - a.modified)
  })

  // ── Pending folder persistence ──────────────────────────────
  function pendingFoldersKey(): string {
    return `unkai.notes.pendingFolders.${accountId}`
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

  function startAddFolder() {
    creatingFolder = true
    newFolderName = ''
  }
  function commitAddFolder() {
    if (!creatingFolder) return
    const path = newFolderName
      .split('/')
      .map((s) => s.trim())
      .filter((s) => s.length > 0)
      .join('/')
    creatingFolder = false
    newFolderName = ''
    // Empty input cancels — matches what blur-on-empty would
    // expect a user to feel.
    if (!path) return
    pendingFolders = new Set(pendingFolders).add(path)
    savePendingFolders()
    selection = { kind: 'category', path }
  }
  function cancelAddFolder() {
    creatingFolder = false
    newFolderName = ''
  }

  /** Notes that live in `path` or any of its sub-folders. */
  function notesInFolderTree(path: string): Note[] {
    return notes.filter(
      (n) => n.category === path || n.category.startsWith(path + '/'),
    )
  }

  /** Open the folder action menu — used by both the three-dot
   *  trigger (positions to the right of the button) and the
   *  right-click handler (positions at the cursor). */
  /** `x`/`y` are layout-space coordinates — callers convert via
   *  `cursorAnchor` / `anchorRect` before handing them over. */
  function openFolderMenu(path: string, x: number, y: number) {
    folderMenu = { path, ...clampToViewport({ x, y }, 200, 80) }
  }

  function startRemoveFolder(path: string) {
    folderMenu = null
    const affected = notesInFolderTree(path)
    if (affected.length === 0) {
      // Pending-only or otherwise empty — just drop the folder
      // from the in-memory pending set.  Nothing destructive,
      // no confirm.
      if (pendingFolders.has(path)) {
        const next = new Set(pendingFolders)
        next.delete(path)
        pendingFolders = next
        savePendingFolders()
      }
      if (
        selection.kind === 'category'
        && (selection.path === path || selection.path.startsWith(path + '/'))
      ) {
        selection = { kind: 'all' }
      }
      return
    }
    folderDeleteConfirm = { path, affectedCount: affected.length }
  }

  async function confirmRemoveFolder() {
    if (!folderDeleteConfirm || !accountId) return
    const path = folderDeleteConfirm.path
    folderOpBusy = true
    try {
      // Walk affected notes once (caching from the closure
      // doesn't help — the list mutates as we update each one).
      const targets = notesInFolderTree(path)
      for (const n of targets) {
        const updated = await api.notes.updateNextcloudNote({
          ncId: accountId,
          noteId: n.id,
          etag: n.etag,
          title: null,
          content: null,
          category: '',
          favorite: null,
        })
        const rest = notes.filter((x) => x.id !== updated.id)
        notes = [updated, ...rest].sort((a, b) => b.modified - a.modified)
      }
      if (pendingFolders.has(path)) {
        const next = new Set(pendingFolders)
        next.delete(path)
        pendingFolders = next
        savePendingFolders()
      }
      if (
        selection.kind === 'category'
        && (selection.path === path || selection.path.startsWith(path + '/'))
      ) {
        selection = { kind: 'all' }
      }
      folderDeleteConfirm = null
    } catch (e) {
      error = formatError(e) || m.notes_view_err_remove_folder()
    } finally {
      folderOpBusy = false
    }
  }

  function cancelRemoveFolder() {
    folderDeleteConfirm = null
  }

  /** Flat alphabetised list of every category currently in
   *  use across the loaded notes, plus the user's pending
   *  folders.  Drives the move-to-folder popover. */
  const allFolderPaths = $derived.by((): string[] => {
    const set = new Set<string>()
    for (const n of notes) {
      if (n.category.trim()) set.add(n.category)
    }
    for (const p of pendingFolders) set.add(p)
    return [...set].sort((a, b) => a.localeCompare(b))
  })

  function startMoveNote(note: Note) {
    movingNote = note
    moveFilter = ''
  }

  /** Centralised re-categorise.  Used by both the move modal and
   *  the drag-and-drop drop handler so the success path (cache
   *  splice + pending-folder promotion + etag refresh) lives in
   *  one place. */
  async function moveNoteToCategory(note: Note, path: string) {
    if (!accountId) return
    if (note.category === path) return
    try {
      const updated = await api.notes.updateNextcloudNote({
        ncId: accountId,
        noteId: note.id,
        etag: note.etag,
        title: null,
        content: null,
        category: path,
        favorite: null,
      })
      const rest = notes.filter((x) => x.id !== updated.id)
      notes = [updated, ...rest].sort((a, b) => b.modified - a.modified)
      if (selectedId === updated.id) draftEtag = updated.etag
      // Promote a pending folder to real once a note actually
      // lives in it.
      if (path && pendingFolders.has(path)) {
        const next = new Set(pendingFolders)
        next.delete(path)
        pendingFolders = next
        savePendingFolders()
      }
    } catch (e) {
      error = formatError(e) || m.notes_view_err_move()
    }
  }

  async function pickMoveTarget(path: string) {
    const note = movingNote
    movingNote = null
    if (!note) return
    await moveNoteToCategory(note, path)
  }

  // ── Drag and drop ────────────────────────────────────────────
  function onNoteDragStart(e: DragEvent, note: Note) {
    if (!e.dataTransfer) return
    e.dataTransfer.effectAllowed = 'move'
    e.dataTransfer.setData(
      NOTE_DND_MIME,
      JSON.stringify({ accountId: note.nextcloud_account_id, noteId: note.id }),
    )
    // Some browsers want a `text/plain` fallback for the drag
    // image / outside-app drag preview.
    e.dataTransfer.setData('text/plain', note.title || m.notes_view_untitled())
  }

  function isNoteDrag(e: DragEvent): boolean {
    const types = Array.from(e.dataTransfer?.types ?? [])
    return types.includes(NOTE_DND_MIME)
  }

  function onFolderDragOver(e: DragEvent, target: string) {
    // Always preventDefault — Webview engines may hide custom
    // MIME types during dragover (privacy feature), and gating
    // this on `isNoteDrag` would forbid the drop.  The drop
    // handler revalidates via getData.
    e.preventDefault()
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move'
    if (isNoteDrag(e)) dragOverFolder = target
  }

  function onFolderDragLeave(target: string) {
    if (dragOverFolder === target) dragOverFolder = null
  }

  async function onFolderDrop(e: DragEvent, target: string) {
    e.preventDefault()
    dragOverFolder = null
    const raw = e.dataTransfer?.getData(NOTE_DND_MIME)
    if (!raw) return
    let payload: { accountId: string; noteId: number }
    try {
      payload = JSON.parse(raw)
    } catch {
      return
    }
    // Only honour drops for notes belonging to the current
    // account — cross-account moves aren't meaningful for the
    // Notes API (each NC server has its own id space).
    if (payload.accountId !== accountId) return
    const note = notes.find((n) => n.id === payload.noteId)
    if (!note) return
    await moveNoteToCategory(note, target)
  }

  /** Quick-action delete — same behaviour as `deleteSelected`
   *  but no confirm dialog (matches the mail quick-delete UX:
   *  the click was deliberate, the recovery path is "create a
   *  new note" since a quick action shouldn't get in the user's
   *  way every time). */
  async function quickDeleteNote(note: Note) {
    if (!accountId) return
    try {
      await api.notes.deleteNextcloudNote({ ncId: accountId, noteId: note.id })
      notes = notes.filter((n) => n.id !== note.id)
      if (selectedId === note.id) {
        selectedId = null
        saveStatus = ''
      }
    } catch (e) {
      error = formatError(e) || m.notes_view_err_delete()
    }
  }

  // Escape dismissal for the move-to-folder modal.  Backdrop
  // click is handled by the modal itself; this is just for
  // keyboard.
  $effect(() => {
    if (!movingNote) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') movingNote = null
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  })

  // Outside-click + Escape dismissal for the folder action menu.
  // setTimeout(..., 0) so the click that *opened* the menu doesn't
  // immediately close it (the document mousedown listener fires
  // *before* the click on the trigger settles).
  $effect(() => {
    if (!folderMenu) return
    const onMouseDown = () => (folderMenu = null)
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') folderMenu = null
    }
    const t = setTimeout(() => {
      document.addEventListener('mousedown', onMouseDown)
      document.addEventListener('keydown', onKey)
    }, 0)
    return () => {
      clearTimeout(t)
      document.removeEventListener('mousedown', onMouseDown)
      document.removeEventListener('keydown', onKey)
    }
  })

  // Auto-focus the inline draft input the moment the row mounts.
  // The effect re-runs when `creatingFolder` flips on; the
  // bind:this is populated by the time the effect commits.
  $effect(() => {
    if (creatingFolder && newFolderInput) {
      newFolderInput.focus()
      newFolderInput.select()
    }
  })

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
      const created = await api.notes.createNextcloudNote({
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
      error = formatError(e) || m.notes_view_err_create()
    }
  }

  async function deleteSelected() {
    if (!accountId || selectedId == null) return
    const note = notes.find((n) => n.id === selectedId)
    if (!note) return
    const label = note.title.trim() || m.notes_view_this_note()
    if (!confirm(m.notes_view_delete_confirm({ title: label }))) return
    try {
      await api.notes.deleteNextcloudNote({ ncId: accountId, noteId: note.id })
      notes = notes.filter((n) => n.id !== note.id)
      selectedId = null
      saveStatus = ''
    } catch (e) {
      error = formatError(e) || m.notes_view_err_delete()
    }
  }

  async function toggleFavorite() {
    if (!accountId || selectedId == null) return
    const note = notes.find((n) => n.id === selectedId)
    if (!note) return
    try {
      const updated = await api.notes.updateNextcloudNote({
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
      error = formatError(e) || m.notes_view_err_favorite()
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
      const updated = await api.notes.updateNextcloudNote({
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

  /** "Send as mail" — opens Compose with the note as the seed.
   *  `bodyAboveSignature` lands the markdown body where the
   *  user would normally be typing, with their signature below
   *  it; without it the recipient gets "(blank) → signature →
   *  note content" which reads as a forwarded artefact. */
  function sendAsMail() {
    const note = notes.find((n) => n.id === selectedId)
    if (!note) return
    oncompose({
      subject: note.title || m.notes_view_untitled_note(),
      body: note.content,
      bodyAboveSignature: true,
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
  <!-- Stacked header (#522) — the standard integration-view shell:
       title above its icon-only actions, docked LEFT so the
       controls stay in the viewing angle near the columns they act
       on.  No header search — the list column carries its own
       SearchInput directly above the rows it filters (the
       master-detail variant, same as ContactsView).  The header
       renders even with zero accounts (actions just disable) so
       its height never changes. -->
  <div class="flex items-center gap-3 px-6 py-3 border-b glass-panel">
    <div class="flex-1 min-w-0 flex flex-col items-start gap-2">
      <h2 class="text-xl font-semibold truncate">{m.notes_view_title()}</h2>
      <div class="flex items-center gap-2 shrink-0">
        <button
          class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center"
          disabled={!accountId}
          onclick={newNote}
          title={m.notes_view_new_note()}
          aria-label={m.notes_view_new_note()}
        ><Icon name="plus" size={14} /></button>
        <button
          class="btn btn-sm preset-tonal-surface inline-flex items-center justify-center"
          disabled={!accountId || syncing}
          onclick={() => void manualSync()}
          title={syncing ? m.notes_view_syncing() : m.notes_view_sync_title()}
          aria-label={syncing ? m.notes_view_syncing() : m.notes_view_sync()}
        ><Icon name={syncing ? 'loading' : 'sync'} size={14} /></button>
      </div>
    </div>
  </div>

  <div class="flex-1 min-h-0 flex">
  {#if accounts.length === 0 && !loading}
    <div class="flex-1 flex items-center justify-center text-sm text-surface-500 p-8 text-center">
      {m.notes_view_no_account()}
    </div>
  {:else}
    <!-- Sidebar: virtuals + folder tree + add-folder.  The New
         note CTA lives in the view header's action slot (#522),
         so the column starts with navigation like the other
         integration views. -->
    <aside
      class="shrink-0 border-r glass-panel flex flex-col text-sm"
      use:resizableSidebar={{ key: 'notes.navSidebar', defaultWidth: 224, min: 160, max: 480 }}
    >
      <div class="flex-1 min-h-0 overflow-y-auto py-2">
        <!-- Virtuals -->
        <button
          class="notes-side-row {selectionMatches(selection, { kind: 'all' }) ? 'is-active' : ''}"
          onclick={() => (selection = { kind: 'all' })}
        >
          <span class="notes-side-icon"><Icon name="notes" size={16} /></span>
          <span class="flex-1 truncate text-left">{m.notes_view_all_notes()}</span>
          <span class="notes-side-count">{totalNotes}</span>
        </button>
        <button
          class="notes-side-row {selectionMatches(selection, { kind: 'favorites' }) ? 'is-active' : ''}"
          onclick={() => (selection = { kind: 'favorites' })}
        >
          <!-- Star is warning-yellow when the row is inactive so
               the Favorites virtual stays semantically obvious in
               the sidebar.  When selected, it inherits the row's
               primary colour like every other tab. -->
          <span
            class="notes-side-icon {selectionMatches(selection, { kind: 'favorites' })
              ? ''
              : 'text-warning-500'}"
          ><Icon name="star" size={16} /></span>
          <span class="flex-1 truncate text-left">{m.notes_view_favorites()}</span>
          <span class="notes-side-count">{favoriteCount}</span>
        </button>
        <button
          class="notes-side-row {selectionMatches(selection, { kind: 'uncategorized' }) ? 'is-active' : ''} {dragOverFolder === '' ? 'is-drop-target' : ''}"
          onclick={() => (selection = { kind: 'uncategorized' })}
          ondragover={(e) => onFolderDragOver(e, '')}
          ondragleave={() => onFolderDragLeave('')}
          ondrop={(e) => void onFolderDrop(e, '')}
        >
          <span class="notes-side-icon text-base font-semibold leading-none">?</span>
          <span class="flex-1 truncate text-left">{m.notes_view_uncategorized()}</span>
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
          {@const menuOpen = folderMenu?.path === node.path}
          <div
            class="notes-side-row group {isSelected ? 'is-active' : ''} {dragOverFolder === node.path ? 'is-drop-target' : ''}"
            style="padding-left: {0.5 + depth * 0.75}rem"
            role="treeitem"
            tabindex="-1"
            aria-selected={isSelected}
            oncontextmenu={(e) => {
              e.preventDefault()
              const p = cursorAnchor(e)
              openFolderMenu(node.path, p.x, p.y)
            }}
            ondragover={(e) => onFolderDragOver(e, node.path)}
            ondragleave={() => onFolderDragLeave(node.path)}
            ondrop={(e) => void onFolderDrop(e, node.path)}
          >
            {#if hasChildren}
              <button
                type="button"
                class="notes-side-caret"
                onclick={(e) => {
                  e.stopPropagation()
                  toggleCollapsed(node.path)
                }}
                aria-label={isCollapsed ? m.notes_view_expand_folder() : m.notes_view_collapse_folder()}
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
              <span class="notes-side-icon"><Icon name="files" size={16} /></span>
              <span class="truncate">{node.name}</span>
            </button>
            {#if node.descendantCount > 0 && !menuOpen}
              <span class="notes-side-count group-hover:hidden">{node.descendantCount}</span>
            {/if}
            <!-- Three-dot trigger.  Mirrors the right-click menu so
                 trackpad / touchscreen users get the same actions
                 (CLAUDE.md: "Three-dot button signals 'this row has
                 actions' / Right-click does the same thing"). -->
            <button
              type="button"
              class="notes-side-more {menuOpen ? 'opacity-100' : 'opacity-0 group-hover:opacity-100 focus:opacity-100'}"
              title={m.notes_view_folder_actions()}
              aria-label={m.notes_view_folder_actions()}
              onclick={(e) => {
                e.stopPropagation()
                const r = anchorRect(e.currentTarget as HTMLElement)
                openFolderMenu(node.path, r.right + 4, r.top)
              }}
            >⋯</button>
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

        {#if creatingFolder}
          <!-- Draft row: matches the regular folder-row layout so
               the new folder feels like it's already in the tree
               while the user names it.  Enter / blur commits,
               Escape cancels (CLAUDE.md inline-rename convention). -->
          <div class="notes-side-row" style="padding-left: 0.5rem">
            <span class="notes-side-caret-spacer"></span>
            <span class="notes-side-icon"><Icon name="files" size={16} /></span>
            <input
              bind:this={newFolderInput}
              bind:value={newFolderName}
              type="text"
              class="notes-side-draft-input"
              placeholder={m.notes_view_new_folder_placeholder()}
              onkeydown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault()
                  commitAddFolder()
                } else if (e.key === 'Escape') {
                  e.preventDefault()
                  cancelAddFolder()
                }
              }}
              onblur={commitAddFolder}
            />
          </div>
        {/if}

        <button
          type="button"
          class="notes-side-add inline-flex items-center justify-center gap-1.5"
          onclick={startAddFolder}
          disabled={!accountId || creatingFolder}
        >
          <Icon name="add-folder" size={14} />
          <span>{m.notes_view_add_folder()}</span>
        </button>
      </div>
    </aside>

    <!-- List pane: account picker + refresh strip + scrollable
         note list.  Account picker only shows when more than one
         NC account is connected; refresh is always there. -->
    <div
      class="shrink-0 border-r border-surface-200 dark:border-surface-700 flex flex-col"
      use:resizableSidebar={{ key: 'notes.listColumn', defaultWidth: 288, min: 220, max: 600 }}
    >
      <!-- Search bar — uses the shared `SearchInput` so the
           magnifier / clear-X chrome stays in sync with every
           other "Search …" surface in the app.  Background sync
           still runs on the polling timer + after every cache
           load, so an explicit refresh button isn't worth its own
           affordance.  Filter is layered on top of the sidebar
           folder filter so search works "within Joplin" /
           "within Favorites" / "across all notes" depending on
           the sidebar pick. -->
      <div class="border-b border-surface-200 dark:border-surface-700 p-2">
        <SearchInput
          bind:value={searchQuery}
          placeholder={m.notes_view_search_placeholder()}
        />
      </div>

      {#if accounts.length > 1}
        <div class="px-3 py-1.5 border-b border-surface-200 dark:border-surface-700">
          <select
            class="select text-xs py-1 px-2 rounded-lg w-full"
            value={accountId}
            onchange={(e) => selectAccount((e.currentTarget as HTMLSelectElement).value)}
          >
            {#each accounts as a (a.id)}
              <option value={a.id}>{a.display_name || a.username}</option>
            {/each}
          </select>
        </div>
      {/if}

      <div class="flex-1 min-h-0 overflow-y-auto">
        {#if loading && notes.length === 0}
          <div class="p-6 text-center text-sm text-surface-500">{m.notes_view_loading()}</div>
        {:else if error && notes.length === 0}
          <div class="p-4 text-sm text-error-500">{error}</div>
        {:else if filteredNotes.length === 0}
          <div class="p-6 text-center text-sm text-surface-500">
            {#if searchQuery.trim()}
              {m.notes_view_no_match({ query: searchQuery.trim() })}
            {:else if selection.kind === 'all'}
              {m.notes_view_empty()}
            {:else}
              {m.notes_view_empty_folder()}
            {/if}
          </div>
        {:else}
          {#each filteredNotes as n (n.id)}
            <div
              class="relative group border-b border-surface-100 dark:border-surface-800"
              draggable="true"
              role="listitem"
              ondragstart={(e) => onNoteDragStart(e, n)}
            >
              <button
                class="w-full text-left px-4 py-3 transition-colors
                  {selectedId === n.id
                    ? 'bg-primary-500/10'
                    : 'hover:bg-primary-500/10'}"
                onclick={() => openNote(n)}
              >
                <div class="flex items-center justify-between mb-1 gap-2">
                  <span class="text-sm font-medium truncate flex-1 inline-flex items-center gap-1">
                    {#if n.favorite}
                      <span class="text-warning-500 shrink-0"><Icon name="star" size={12} /></span>
                    {/if}
                    <span class="truncate">{n.title || m.notes_view_untitled()}</span>
                  </span>
                  <span class="text-xs text-surface-500 shrink-0">{fmtDate(n.modified)}</span>
                </div>
                {#if preview(n)}
                  <p class="text-xs text-surface-500 truncate">{preview(n)}</p>
                {/if}
                {#if n.category}
                  <p class="text-[10px] text-surface-400 mt-1 truncate inline-flex items-center gap-1">
                    <Icon name="files" size={11} />{n.category}
                  </p>
                {/if}
              </button>

              <!-- Quick actions — same shape and treatment as the
                   mail-list quick-action cluster (#138 follows the
                   pattern at MailList.svelte ≈ line 950).  Shown
                   bottom-right so they don't overlap the modified
                   timestamp; pointer-events disabled when hidden so
                   the row's primary click still hits the area. -->
              <div
                class="absolute right-1 bottom-2 flex items-center gap-0.5 opacity-0 pointer-events-none transition-opacity
                       group-hover:opacity-100 group-hover:pointer-events-auto
                       focus-within:opacity-100 focus-within:pointer-events-auto"
              >
                <button
                  type="button"
                  class="w-7 h-7 rounded-lg flex items-center justify-center quick-action-btn shadow-sm"
                  title={m.notes_view_move_to_folder()}
                  aria-label={m.notes_view_move_to_folder()}
                  onclick={(e) => {
                    e.stopPropagation()
                    startMoveNote(n)
                  }}
                >
                  <Icon name="move-to-folder" size={16} />
                </button>
                <button
                  type="button"
                  class="w-7 h-7 rounded-lg flex items-center justify-center quick-action-btn quick-action-btn-danger shadow-sm"
                  title={m.notes_view_delete()}
                  aria-label={m.notes_view_delete()}
                  onclick={(e) => {
                    e.stopPropagation()
                    void quickDeleteNote(n)
                  }}
                >
                  <Icon name="trash" size={16} />
                </button>
              </div>
            </div>
          {/each}
        {/if}
      </div>
    </div>

    <!-- Editor pane -->
    <div class="flex-1 min-w-0 flex flex-col">
        {#if selectedId == null}
          <div class="flex-1 flex items-center justify-center text-sm text-surface-500">
            {m.notes_view_editor_empty()}
          </div>
        {:else}
          {@const open = notes.find((n) => n.id === selectedId)}
          {#if open}
            <div class="px-5 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
              <input
                class="input flex-1 text-base font-semibold px-3 py-2 rounded-lg"
                placeholder={m.notes_view_title_placeholder()}
                bind:value={draftTitle}
                oninput={scheduleSave}
              />
              {#if saveStatus === 'saving'}
                <span class="text-xs text-surface-400">{m.notes_view_saving()}</span>
              {:else if saveStatus === 'saved'}
                <span class="text-xs text-success-500">{m.notes_view_saved()}</span>
              {:else if saveStatus === 'error'}
                <span class="text-xs text-error-500">{m.notes_view_save_failed()}</span>
              {/if}
              <!-- Icon-only action row on the project's single
                   outlined-surface base (CLAUDE.md button
                   vocabulary): the star's fill colour carries the
                   favorite state, the eye toggles the rendered
                   preview, and Delete stays neutral at rest with
                   the red hover overlay marking it destructive. -->
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                onclick={toggleFavorite}
                title={open.favorite ? m.notes_view_unstar() : m.notes_view_star()}
                aria-label={open.favorite ? m.notes_view_unstar() : m.notes_view_star()}
                aria-pressed={open.favorite}
              >
                <Icon
                  name="star"
                  size={14}
                  class={open.favorite ? 'text-warning-500' : ''}
                />
              </button>
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                onclick={() => (showPreview = !showPreview)}
                title={showPreview ? m.notes_view_preview_hide() : m.notes_view_preview_show()}
                aria-label={showPreview ? m.notes_view_preview_hide() : m.notes_view_preview_show()}
                aria-pressed={showPreview}
              >
                <Icon name={showPreview ? 'eye-off' : 'eye'} size={14} />
              </button>
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                onclick={sendAsMail}
                title={m.notes_view_send_as_mail()}
                aria-label={m.notes_view_send_as_mail_aria()}
              >
                <Icon name="email-envelope" size={14} />
              </button>
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-error-500/15 hover:text-error-500 hover:border-error-500/40"
                onclick={deleteSelected}
                title={m.notes_view_delete_note()}
                aria-label={m.notes_view_delete_note()}
              >
                <Icon name="trash" size={14} />
              </button>
            </div>

            <NotesMarkdownEditor
              bind:value={draftContent}
              onchange={() => scheduleSave()}
              {showPreview}
              accountId={mailAccountId}
              {onopenmail}
              onmailto={(init) => oncompose(init)}
            />
          {/if}
        {/if}
    </div>
  {/if}
  </div>
</div>

<!-- Move-to-folder modal — same shape as `MoveFolderPicker.svelte`
     in the mail UI: centered backdrop dialog with a filter input
     and a scrollable folder list.  An "Uncategorized" entry sits
     at the top so the user can clear a category in one click. -->
{#if movingNote}
  {@const currentCat = movingNote.category}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    aria-label={m.notes_view_move_to_folder()}
    tabindex="-1"
    onclick={(e) => {
      if (e.target === e.currentTarget) movingNote = null
    }}
  >
    <div class="glass-float rounded-2xl flex flex-col w-[420px] max-w-[90vw] max-h-[80vh]">
      <header class="px-5 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center justify-between">
        <h2 class="text-base font-semibold">{m.notes_view_move_to_folder()}</h2>
        <button
          class="text-surface-500 hover:text-surface-900 dark:hover:text-surface-100"
          onclick={() => (movingNote = null)}
          aria-label={m.notes_view_close()}
        >✕</button>
      </header>

      <div class="px-3 py-2 border-b border-surface-200 dark:border-surface-700">
        <input
          type="text"
          class="input w-full text-sm px-2 py-1 rounded-lg"
          placeholder={m.notes_view_filter_folders()}
          bind:value={moveFilter}
        />
      </div>

      <div class="flex-1 overflow-y-auto px-2 py-2">
        {#if allFolderPaths.length === 0 && currentCat === ''}
          <p class="px-3 py-2 text-xs text-surface-500">
            {m.notes_view_move_no_folders()}
          </p>
        {:else}
          <!-- Uncategorized first; disabled when the note is
               already there so the user can't move-to-self. -->
          <button
            class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm text-left transition-colors disabled:text-surface-400 disabled:cursor-not-allowed
              {currentCat === ''
                ? 'bg-surface-200/50 dark:bg-surface-700/50'
                : 'hover:bg-primary-500/10'}"
            disabled={currentCat === ''}
            onclick={() => pickMoveTarget('')}
            title={currentCat === '' ? m.notes_view_already_uncategorized() : m.notes_view_move_to_uncategorized()}
          >
            <span class="text-base font-semibold leading-none w-4 text-center">?</span>
            <span class="flex-1">{m.notes_view_uncategorized()}</span>
          </button>

          {@const filteredPaths = (() => {
            const q = moveFilter.trim().toLowerCase()
            return q
              ? allFolderPaths.filter((p) => p.toLowerCase().includes(q))
              : allFolderPaths
          })()}

          {#if filteredPaths.length > 0}
            <div class="my-1 border-t border-surface-200 dark:border-surface-700"></div>
            {#each filteredPaths as path (path)}
              {@const isCurrent = path === currentCat}
              <button
                class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm text-left transition-colors disabled:text-surface-400 disabled:cursor-not-allowed
                  {isCurrent
                    ? 'bg-surface-200/50 dark:bg-surface-700/50'
                    : 'hover:bg-primary-500/10'}"
                disabled={isCurrent}
                onclick={() => pickMoveTarget(path)}
                title={isCurrent ? m.notes_view_already_in_folder() : m.notes_view_move_to_path({ path })}
              >
                <Icon name="files" size={16} class="shrink-0" />
                <span class="flex-1 truncate">{path}</span>
              </button>
            {/each}
          {:else if moveFilter.trim()}
            <p class="px-3 py-2 text-xs text-surface-500">{m.notes_view_move_no_match()}</p>
          {/if}
        {/if}
      </div>
    </div>
  </div>
{/if}

<!-- Folder action menu — shared between the three-dot trigger
     and right-click.  `position: fixed` so it floats above the
     sidebar's overflow:auto without getting clipped. -->
{#if folderMenu}
  <div
    class="fixed z-60 min-w-44 rounded-xl glass-float py-1 text-sm"
    style="left: {folderMenu.x}px; top: {folderMenu.y}px;"
    role="menu"
    tabindex="-1"
    onmousedown={(e) => e.stopPropagation()}
  >
    <button
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-error-500/10 text-error-600 dark:text-error-400 disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={folderOpBusy}
      onclick={() => startRemoveFolder(folderMenu!.path)}
    >
      <Icon name="delete-folder" size={16} />
      <span>{m.notes_view_remove_folder()}</span>
    </button>
  </div>
{/if}

<!-- Confirmation modal for destructive folder removal.  Pending
     folders skip this — they have no notes attached, so dropping
     them is a no-op the user shouldn't have to confirm. -->
{#if folderDeleteConfirm}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => {
      if (e.target === e.currentTarget) cancelRemoveFolder()
    }}
  >
    <div class="glass-float rounded-2xl p-5 max-w-md w-full mx-4">
      <h2 class="text-base font-semibold mb-2">
        {m.notes_view_folder_delete_title({ path: folderDeleteConfirm.path })}
      </h2>
      <p class="text-sm text-surface-600 dark:text-surface-300 mb-4">
        {folderDeleteConfirm.affectedCount === 1
          ? m.notes_view_folder_delete_body_one()
          : m.notes_view_folder_delete_body_many({ count: folderDeleteConfirm.affectedCount })}
      </p>
      <!-- Labelled buttons deliberately — a destructive confirm
           should read as words, not glyphs (same trade the
           contact delete confirm makes). -->
      <div class="flex items-center justify-end gap-2">
        <button
          class="btn btn-sm preset-tonal"
          onclick={cancelRemoveFolder}
          disabled={folderOpBusy}
        >{m.notes_view_cancel()}</button>
        <button
          class="btn btn-sm preset-filled-error-500"
          onclick={confirmRemoveFolder}
          disabled={folderOpBusy}
        >{folderOpBusy ? m.notes_view_removing() : m.notes_view_remove()}</button>
      </div>
    </div>
  </div>
{/if}

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
  :global(.notes-side-row.is-drop-target) {
    background: rgba(var(--color-primary-500) r g b / 0.2);
    box-shadow: inset 0 0 0 2px var(--color-primary-500);
    border-radius: 0.5rem;
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
  :global(.notes-side-more) {
    width: 1.25rem;
    height: 1.25rem;
    flex-shrink: 0;
    border-radius: 0.5rem;
    color: var(--color-surface-500);
    background: transparent;
    border: none;
    cursor: pointer;
    line-height: 1;
    font-size: 0.875rem;
    margin-left: 0.25rem;
    transition: opacity 80ms ease;
  }
  :global(.notes-side-more:hover) {
    background: var(--color-surface-200);
  }
  :global([data-mode='dark'] .notes-side-more:hover) {
    background: var(--color-surface-700);
  }

  :global(.notes-side-draft-input) {
    flex: 1;
    min-width: 0;
    background: transparent;
    border: none;
    outline: none;
    font-size: 0.8125rem;
    color: inherit;
    padding: 0;
  }
  :global(.notes-side-draft-input::placeholder) {
    color: var(--color-surface-400);
    font-style: italic;
  }
  :global(.notes-side-add) {
    width: calc(100% - 1rem);
    margin: 0.5rem 0.5rem 0;
    padding: 0.375rem 0.5rem;
    border-radius: 0.5rem;
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

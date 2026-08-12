<script lang="ts">
  /**
   * Sidebar — mail-view only: Compose CTA + folder list for the
   * currently active account.
   *
   * The shell-level nav (account avatars, integrations, settings)
   * lives in `IconRail.svelte` now; this component is mounted by
   * App.svelte exclusively when the user is in the mail view
   * (`currentView === 'inbox'`). That lets the folder list extend
   * floor-to-ceiling in its column and keeps the sidebar focused on
   * a single job — telling the user where their mail lives.
   *
   * Manual refresh is gone from here too: the background sync loop
   * runs every `background_sync_interval_secs` and a poll fires
   * automatically whenever the user enters the mail view (see the
   * `currentView` effect in App.svelte). The explicit "Check Mail
   * Now" button now lives inside Settings, mirroring the
   * Contacts / Calendar sync-now buttons.
   */

  import * as api from './api'
  import { anchorRect, clampToViewport, cursorAnchor } from './coords'
  import { formatError } from './errors'
  import EmojiPicker from './EmojiPicker.svelte'
  import Icon, { type IconName } from './Icon.svelte'
  import { resizableSidebar } from './resizableSidebar'
  import { buildFolderRows } from './folderTree'
  import { m } from '../paraglide/messages'
  import {
    UNIFIED_SENT_FOLDER,
    UNIFIED_DRAFTS_FOLDER,
    UNIFIED_JUNK_FOLDER,
    UNIFIED_ARCHIVE_FOLDER,
    UNIFIED_TRASH_FOLDER,
  } from './unifiedFolders'

  interface Folder {
    name: string
    delimiter: string | null
    attributes: string[]
    unread_count: number | null
  }

  /** One "folder name contains X → use icon Y" rule, mirror of the
      Rust `FolderIconRule` struct. Carried inside `Account` so the
      sidebar can apply per-account theming without a separate fetch. */
  interface FolderIconRule {
    keyword: string
    icon: string
  }

  /** Slim account row — only the folder-icon rules matter to the
      Sidebar now that account switching has moved to the IconRail.
      `folder_icon_overrides` is the per-folder picker state: maps
      the full folder path to the user's chosen emoji and beats every
      other icon source when it exists. */
  interface Account {
    id: string
    display_name: string
    email: string
    folder_icons?: FolderIconRule[]
    folder_icon_overrides?: Record<string, string>
  }

  interface Props {
    accounts?: Account[]
    accountId: string
    selectedFolder: string
    /** Bumped by the parent to force a cache-only re-read (manual
     *  refresh, mark-as-read, new-mail signal). */
    refreshToken?: number
    /** Unified-inbox mode. When true the per-account folder tree
     *  collapses to a single "All Inboxes" entry; toggled on / off
     *  via the IconRail's "ALL" bubble. */
    unified?: boolean
    onselectfolder: (name: string) => void
    oncompose?: () => void
    /** Called after the Sidebar has mutated an account record on
     *  the backend (currently only: `set_folder_icon`). The parent
     *  re-fetches its `accounts` state so the updated overrides
     *  map flows back into the Sidebar's `accounts` prop on the
     *  next render. */
    onaccountschanged?: () => void
    /** Fires after a drag-and-drop move (#89) succeeds.  Parent
     *  uses it to drop the source-folder envelope from `MailList`
     *  and trigger the auto-advance flow (#99) — the moved UID
     *  flows in here so it can pick the next neighbour. */
    onmessagemoved?: (removedUid: number) => void
    /** Fires when the optimistic drag-drop move finally errored
     *  on the IMAP side (#174 follow-up).  The cache rows have
     *  been un-tombstoned by the backend, so the parent should
     *  bump `refreshToken` to make MailList re-pull from cache —
     *  the failed rows then reappear.  Optional: when omitted,
     *  failures still show the error banner but the UI stays
     *  out-of-sync until the next manual refresh. */
    onmovesfailed?: () => void
    /** Total queued rows in the local Outbox (#276) across every
     *  account.  When > 0, a synthetic "Outbox" folder is rendered
     *  above the real IMAP folders with this number as its badge —
     *  selecting it routes the MailList to its Outbox variant.
     *  When 0, the synthetic folder is hidden so a healthy install
     *  with nothing queued sees the same sidebar it always did. */
    outboxCount?: number
  }
  let {
    accounts = [],
    accountId,
    selectedFolder,
    refreshToken = 0,
    unified = false,
    onselectfolder,
    oncompose,
    onaccountschanged,
    onmessagemoved,
    onmovesfailed,
    outboxCount = 0,
  }: Props = $props()

  /** Sentinel folder name used to route Outbox selection through
   *  the same `selectedFolder` channel as IMAP folders.  Picked
   *  with a leading underscore so an IMAP server can't return a
   *  real folder that collides — RFC 6855 / RFC 3501 don't
   *  forbid the name, but no production server we'd ship against
   *  uses it.  Kept in this module rather than sprinkled across
   *  files so the constant is the single source of truth. */
  const OUTBOX_FOLDER = 'Outbox'

  let folders = $state<Folder[]>([])
  let loading = $state(true)
  let error = $state('')

  // ── Drag-and-drop drop targets (#89) ───────────────────────────
  // Folder rows accept drags from `MailList` carrying our private
  // `application/x-unkai-mail` payload.  `dragOverFolder` drives a
  // subtle highlight on the hovered row.  We swallow drops onto the
  // current source folder (`folder === payload.folder`) so a misfire
  // doesn't trip the IMAP server with a move-to-self request.
  let dragOverFolder = $state<string | null>(null)

  /** Best-effort "is this our drag?" check.  Used to gate the
   *  hover highlight so a generic file drag doesn't paint the
   *  folder.  We deliberately do **not** gate `preventDefault`
   *  on this — see `onFolderDragOver` for why. */
  function isMailDrag(e: DragEvent): boolean {
    // `types` is sometimes a `DOMStringList`, sometimes an
    // array; `Array.from` normalises both shapes so the `.some`
    // call below works either way.
    const types = Array.from(e.dataTransfer?.types ?? [])
    return types.includes('application/x-unkai-mail')
  }

  function onFolderDragOver(e: DragEvent, target: Folder) {
    // The synthetic Outbox folder (#276) is local-only and
    // doesn't exist on the IMAP server, so dropping a mail row
    // onto it has no sensible meaning — `move_messages` would
    // fail with "no such folder".  Reject the drop and skip the
    // hover highlight.
    if (target.name === OUTBOX_FOLDER) {
      if (e.dataTransfer) e.dataTransfer.dropEffect = 'none'
      return
    }
    // ALWAYS preventDefault so the drop is permitted, regardless
    // of whether we can prove this is our drag.  Several webview
    // engines (Edge WebView2 in particular) hide custom MIME
    // types from `dataTransfer.types` during `dragover` as a
    // privacy feature — the data is only revealed at drop time.
    // Gating preventDefault on `isMailDrag` therefore caused
    // every drop to register as forbidden in those engines.
    // The drop handler revalidates via `getData(...)` and bails
    // for non-mail payloads, so accepting the drag here is
    // safe.
    e.preventDefault()
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move'
    // The visual highlight stays gated — we only paint the
    // hovered-folder treatment when we *can* tell this is one
    // of our drags.  When the engine has hidden the type, we
    // still allow the drop but the row doesn't pulse.
    if (isMailDrag(e)) dragOverFolder = target.name
  }

  function onFolderDragLeave(target: Folder) {
    if (dragOverFolder === target.name) dragOverFolder = null
  }

  async function onFolderDrop(e: DragEvent, target: Folder) {
    dragOverFolder = null
    // Always preventDefault on drop too, even when the payload
    // turns out not to be ours — that stops the browser from
    // navigating to a dropped URL or opening a dropped file.
    e.preventDefault()
    // Outbox is local-only (#276) — bail before parsing any
    // payload so the drop is a hard no-op.
    if (target.name === OUTBOX_FOLDER) return
    const raw = e.dataTransfer?.getData('application/x-unkai-mail')
    // No payload → not our drag (or the engine forgot to
    // hand it over); silently bail.
    if (!raw) return
    // Payload is always an array; multi-select drags carry several
    // entries, single-row drags carry one.
    let payload: { accountId: string; folder: string; uid: number }[]
    try {
      const parsed = JSON.parse(raw)
      payload = Array.isArray(parsed) ? parsed : [parsed]
    } catch {
      return
    }
    // Group by (accountId, sourceFolder) so each group can ride a
    // single batched IMAP MOVE on the backend.  Looping `invoke`
    // per-UID in JS opened a fresh IMAP connection every time, and
    // some servers were dropping the last move in the burst due to
    // rapid connection recycling — `move_messages` does the whole
    // group in one COPY + STORE + EXPUNGE round-trip on a single
    // session, which both fixes the race and is much faster.
    const groups = new Map<
      string,
      { accountId: string; folder: string; uids: number[] }
    >()
    for (const item of payload) {
      if (target.name === item.folder) continue // move-to-self
      // NUL separator — the one byte that can't legally appear in
      // an account id or IMAP folder name, so composite keys can't
      // collide (`"a" + "b/c"` vs `"a/b" + "c"`).
      const key = `${item.accountId}\0${item.folder}`
      const existing = groups.get(key)
      if (existing) existing.uids.push(item.uid)
      else
        groups.set(key, {
          accountId: item.accountId,
          folder: item.folder,
          uids: [item.uid],
        })
    }

    // Optimistic: drop the moved rows from the parent's bound
    // envelope list immediately so the user sees them disappear
    // the moment they release the mouse, regardless of how long
    // the IMAP MOVE takes (#174 follow-up).  The backend's
    // `move_messages` IPC tombstones the cache rows, so even a
    // folder switch mid-flight stays consistent.  Failure
    // recovery: the backend's `clear_message_pending` un-
    // tombstones any failed UIDs; we ask the parent to refresh
    // via `onmovesfailed` so MailList re-loads from cache and
    // the failed rows reappear.
    const total = payload.length
    for (const item of payload) {
      if (target.name !== item.folder) {
        onmessagemoved?.(item.uid)
      }
    }
    void (async () => {
      const succeeded: number[] = []
      const failures: unknown[] = []
      for (const g of groups.values()) {
        try {
          const moved = await api.mail.moveMessages({
            accountId: g.accountId,
            folder: g.folder,
            uids: g.uids,
            destFolder: target.name,
          })
          succeeded.push(...moved)
        } catch (err) {
          console.warn('move_messages via drag-and-drop failed', err)
          failures.push(err)
        }
      }
      if (failures.length > 0) {
        error =
          succeeded.length === 0
            ? formatError(failures[0]) || 'Failed to move message'
            : `Moved ${succeeded.length} of ${total} messages — ${failures.length} group(s) failed.`
        onmovesfailed?.()
      }
    })()
  }

  // ── Folder-management state ─────────────────────────────────
  // Each of the three actions (new, rename, delete) owns a single
  // `$state` slot that's either null (idle) or a small object
  // describing the in-flight operation. Only one operation can be
  // active at a time — triggering any of them nulls out the others.
  //
  // Keeping this inline beats a separate component: the operations
  // mutate the same `folders` array the sidebar already owns, the
  // context menu's positioning is trivial, and the confirm dialog
  // is a handful of lines. Extract if / when a third surface needs
  // the same machinery.

  /** Right-click context menu. Null = hidden; otherwise
   *  `{folder, x, y}` anchors the popup at the click position. */
  let contextMenu = $state<{ folder: Folder; x: number; y: number } | null>(null)

  /** Which folder is currently being renamed inline. `null` = no
   *  rename in progress. The row's text swaps to an input while
   *  this matches `folder.name`. */
  let renamingFolder = $state<string | null>(null)
  let renameValue = $state('')

  /** "Create new folder" input. `parent = null` = top-level,
   *  `parent = "INBOX/Projects"` = subfolder under that. The input
   *  renders at the end of the folder list while this is non-null. */
  let newFolderInput = $state<{ parent: string | null; value: string } | null>(null)

  /** "Are you sure?" modal for destructive delete. Null when
   *  hidden. */
  let deleteConfirm = $state<{ folder: Folder } | null>(null)

  /** "Are you sure?" modal for the spam-folder wipe (#483). Null
   *  when hidden. Separate from `deleteConfirm` because the two
   *  actions destroy different things (the folder's *messages* vs
   *  the folder itself) and need distinct copy. */
  let clearSpamConfirm = $state<{ folder: Folder } | null>(null)

  /** Busy flag shared across the three mutations — disables the
   *  context-menu actions and the confirm button while an IMAP
   *  command is in flight to keep the user from double-submitting. */
  let folderOpBusy = $state(false)
  let folderOpError = $state('')

  /** Emoji-picker modal state. `null` = hidden; otherwise the
   *  folder whose icon is being changed. The picker's free-text
   *  input lives in its own `$state` so a cancel/close reliably
   *  wipes it regardless of how the modal is dismissed. */
  let iconPicker = $state<{ folder: Folder } | null>(null)

  function openIconPicker(folder: Folder) {
    iconPicker = { folder }
  }

  function closeIconPicker() {
    iconPicker = null
  }

  /** Persist the icon choice via `set_folder_icon`. `emoji === null`
   *  clears any existing override, restoring the folder to the
   *  default resolution chain (special-use → keyword rule → 📁).
   *  On success the parent re-fetches accounts so the new override
   *  flows back into the `accounts` prop and the next render
   *  paints it. */
  async function commitFolderIcon(folder: Folder, emoji: string | null) {
    folderOpBusy = true
    try {
      await api.mail.setFolderIcon({
        accountId,
        folderName: folder.name,
        icon: emoji,
      })
      onaccountschanged?.()
      closeIconPicker()
    } catch (e) {
      folderOpError = formatError(e) || 'Failed to set folder icon'
    } finally {
      folderOpBusy = false
    }
  }

  /** Close the context menu. Safe to call when already closed.
   *  Also clears any transient error left over from a prior
   *  operation's feedback so the next right-click starts clean. */
  function closeContextMenu() {
    contextMenu = null
    folderOpError = ''
  }

  /** Close-on-click-outside for the context menu. Attached at the
   *  document level while the menu is open; torn down as soon as
   *  it closes so we're not holding a listener during idle time. */
  $effect(() => {
    if (!contextMenu) return
    const onDocMouseDown = (e: MouseEvent) => {
      // Clicks *inside* the menu get `stopPropagation` on the
      // menu's own `onmousedown`, so anything reaching document
      // is by definition outside.
      closeContextMenu()
      void e
    }
    const onDocKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') closeContextMenu()
    }
    document.addEventListener('mousedown', onDocMouseDown)
    document.addEventListener('keydown', onDocKey)
    return () => {
      document.removeEventListener('mousedown', onDocMouseDown)
      document.removeEventListener('keydown', onDocKey)
    }
  })

  function openContextMenu(e: MouseEvent, folder: Folder) {
    e.preventDefault()
    // Cancel any other in-flight edits — opening the menu from a
    // fresh row should clear the noise.
    renamingFolder = null
    newFolderInput = null
    contextMenu = { folder, ...clampToViewport(cursorAnchor(e), 200, 220) }
  }

  /** Join a parent path with a child segment using the parent's
   *  delimiter (or `/` if the server didn't advertise one).
   *  Handles the `parent == null` case for top-level creations. */
  function joinPath(parent: Folder | null, child: string): string {
    if (!parent) return child
    const delim = parent.delimiter ?? '/'
    return `${parent.name}${delim}${child}`
  }

  async function commitRename() {
    // Re-entrancy guard: Enter→commit + the subsequent blur (when
    // unmounting the input or when the user clicks away during the
    // in-flight invoke) can each fire commit, producing two
    // parallel RENAMEs — the first succeeds, the second hits a
    // stale target and errors. `folderOpBusy` stays `true` for
    // the duration of the first call, which short-circuits the
    // duplicate cleanly.
    if (!renamingFolder || folderOpBusy) return
    const oldName = renamingFolder
    const newLeaf = renameValue.trim()
    if (!newLeaf || newLeaf === displayNameFromPath(oldName)) {
      // Nothing changed (or empty) — just bail, no IMAP round-trip.
      renamingFolder = null
      renameValue = ''
      return
    }
    // Rename preserves the parent path; only the last segment
    // changes. That matches what every mail client does and keeps
    // the server-side move simple.
    const parent = parentPath(oldName)
    const newName = parent ? `${parent}${delimiterFor(oldName)}${newLeaf}` : newLeaf
    folderOpBusy = true
    try {
      await api.mail.renameFolder({
        accountId,
        oldName,
        newName,
      })
      // Follow the selection if the user was standing on the
      // renamed folder — otherwise the mail-list column would
      // silently snap to an empty view.
      if (selectedFolder === oldName) onselectfolder(newName)
      renamingFolder = null
      renameValue = ''
      await load(accountId)
    } catch (e) {
      folderOpError = formatError(e) || 'Failed to rename folder'
    } finally {
      folderOpBusy = false
    }
  }

  function cancelRename() {
    renamingFolder = null
    renameValue = ''
  }

  async function commitNewFolder() {
    // Same re-entrancy guard as `commitRename` — Enter + blur can
    // both fire commit during a single in-flight invoke, yielding
    // a second CREATE that gets the server's ALREADYEXISTS error
    // even though the first CREATE succeeded. `folderOpBusy`
    // short-circuits the second call until the first settles.
    if (!newFolderInput || folderOpBusy) return
    const leaf = newFolderInput.value.trim()
    if (!leaf) {
      newFolderInput = null
      return
    }
    const parentFolder =
      newFolderInput.parent === null
        ? null
        : folders.find((f) => f.name === newFolderInput!.parent) ?? null
    const name = joinPath(parentFolder, leaf)
    folderOpBusy = true
    try {
      await api.mail.createFolder({ accountId, name })
      newFolderInput = null
      await load(accountId)
    } catch (e) {
      folderOpError = formatError(e) || 'Failed to create folder'
    } finally {
      folderOpBusy = false
    }
  }

  function cancelNewFolder() {
    newFolderInput = null
  }

  async function confirmDelete() {
    if (!deleteConfirm) return
    const { folder } = deleteConfirm
    folderOpBusy = true
    try {
      await api.mail.deleteFolder({ accountId, name: folder.name })
      // If the user was viewing the folder they just deleted, bounce
      // them to INBOX — otherwise MailList keeps trying to fetch
      // from a mailbox the server no longer has.
      if (selectedFolder === folder.name) onselectfolder('INBOX')
      deleteConfirm = null
      await load(accountId)
    } catch (e) {
      folderOpError = formatError(e) || 'Failed to delete folder'
    } finally {
      folderOpBusy = false
    }
  }

  function cancelDelete() {
    deleteConfirm = null
    folderOpError = ''
  }

  /** Wipe every message out of the spam folder (#483). Permanent —
   *  gated behind the `clearSpamConfirm` modal. The backend clears
   *  the cache rows and fires `mail-flags-updated` +
   *  `unread-count-updated` on success, so MailList empties itself
   *  via the parent's refresh token; we only re-read the folder
   *  badges here for instant feedback. */
  async function confirmClearSpam() {
    if (!clearSpamConfirm || folderOpBusy) return
    const { folder } = clearSpamConfirm
    folderOpBusy = true
    try {
      await api.mail.clearFolder({ accountId, folder: folder.name })
      clearSpamConfirm = null
      await reloadCachedFolders(accountId)
    } catch (e) {
      folderOpError = formatError(e) || m.sidebar_clear_spam_failed()
    } finally {
      folderOpBusy = false
    }
  }

  function cancelClearSpam() {
    clearSpamConfirm = null
    folderOpError = ''
  }

  /** Mark every message in a folder as read (#483). Non-destructive
   *  and reversible per-message, so no confirm modal — it fires
   *  straight from the context menu. Errors surface via the inline
   *  `folderOpError` strip under the folder list. */
  async function markAllRead(folder: Folder) {
    folderOpBusy = true
    try {
      await api.mail.markFolderRead({ accountId, folder: folder.name })
      await reloadCachedFolders(accountId)
    } catch (e) {
      folderOpError = formatError(e) || m.sidebar_mark_all_read_failed()
    } finally {
      folderOpBusy = false
    }
  }

  /** Extract just the last segment of an IMAP folder path, using
   *  the folder's own delimiter when we have it. For the INBOX
   *  case the display name is already a single token. */
  function displayNameFromPath(name: string): string {
    if (name.toUpperCase() === 'INBOX') return 'Inbox'
    const f = folders.find((x) => x.name === name)
    const delim = f?.delimiter ?? '/'
    const parts = name.split(delim)
    return parts[parts.length - 1] || name
  }

  /** The parent path portion of a folder name, or `null` for
   *  top-level. `"INBOX/Projects/2026"` → `"INBOX/Projects"`,
   *  `"INBOX"` → `null`. */
  function parentPath(name: string): string | null {
    const f = folders.find((x) => x.name === name)
    const delim = f?.delimiter ?? '/'
    const idx = name.lastIndexOf(delim)
    return idx < 0 ? null : name.slice(0, idx)
  }

  /** Best-guess delimiter for a folder's subtree. Falls back to
   *  `/` when the server didn't advertise one on the LIST response
   *  (rare but possible for a freshly-created top-level folder). */
  function delimiterFor(name: string): string {
    const f = folders.find((x) => x.name === name)
    return f?.delimiter ?? '/'
  }

  // Total unread across every account's INBOX — used as the badge on
  // the "All Inboxes" entry when unified mode is on. Pulled + kept
  // fresh via the same `unread-count-updated` event the tray listens
  // to, so a poll that changes the total nudges us to re-read it.
  let unifiedUnread = $state(0)
  async function refreshUnifiedUnread() {
    try {
      unifiedUnread = await api.mail.getTotalUnread()
    } catch (e) {
      console.warn('get_total_unread failed:', e)
    }
  }

  $effect(() => {
    void refreshUnifiedUnread()
    let unlisten: (() => void) | null = null
    ;(async () => {
      unlisten = await api.onAppEvent('unread-count-updated', () => {
        void refreshUnifiedUnread()
        // Per-folder badges read from the cached `folders` table,
        // which `mark_envelope_read` and `bump_folder_unread` keep in
        // sync with mail activity. Re-read the cache here so the
        // sidebar picks up those changes without a fetch_folders
        // round-trip per poll.
        void reloadCachedFolders(accountId)
      })
    })()
    return () => {
      unlisten?.()
    }
  })

  /** Cache-only re-read used by the unread-count event listener.
      Full `load()` also fires `fetch_folders`, which is expensive —
      reserved for mount + account switch. */
  async function reloadCachedFolders(id: string) {
    try {
      const cached = await api.mail.getCachedFolders({ accountId: id })
      if (id === accountId) folders = cached
    } catch (e) {
      console.warn('reloadCachedFolders failed:', e)
    }
  }

  // Full reload (cache + network `STATUS` per folder) on mount and
  // whenever the active account switches. We deliberately do *not*
  // tie this to `refreshToken`: that token also bumps on mark-as-read
  // and new-mail signals, and a STATUS round-trip per folder there
  // would (a) swamp the IMAP server on every read and (b) race with
  // our cache decrement — STATUS may return the pre-`\Seen` count if
  // the server hasn't finished propagating it, then `upsert_folders`
  // would overwrite our just-decremented cache count and the badge
  // would visibly snap back to the old number.
  $effect(() => {
    void load(accountId)
  })

  // Cache-only reload on every other refresh signal. The cache stays
  // correct via `mark_envelope_read` (decrements on read) and
  // `bump_folder_unread` (increments on poll), so re-reading from the
  // cache picks up those changes without a network round-trip.
  $effect(() => {
    refreshToken
    void reloadCachedFolders(accountId)
  })

  async function load(id: string) {
    loading = true
    error = ''

    try {
      const cached = await api.mail.getCachedFolders({ accountId: id })
      if (id === accountId) {
        folders = cached
        if (cached.length > 0) loading = false
      }
    } catch (e) {
      console.warn('get_cached_folders failed:', e)
    }

    try {
      const fresh = await api.mail.fetchFolders({ accountId: id })
      if (id === accountId) {
        folders = fresh
      }
    } catch (e) {
      if (folders.length === 0) {
        error = formatError(e) || 'Failed to load folders'
      } else {
        console.warn('fetch_folders failed (showing cached):', e)
      }
    } finally {
      loading = false
    }
  }

  /** True when an IMAP folder is the trash or junk bin. Used both
      for icon selection and for hiding the unread-count badge —
      surfacing "unread" counts there is noise. Recognises the IMAP
      special-use attributes and common name fallbacks (many German
      hosters return `Trash` / `Spam` / `Papierkorb` without flags). */
  function isTrashOrJunk(f: Folder): boolean {
    const name = f.name.toLowerCase()
    const attrs = f.attributes.map((a) => a.toLowerCase())
    const has = (k: string) => attrs.some((a) => a.includes(k))
    return (
      has('trash') ||
      has('deleted') ||
      has('junk') ||
      has('spam') ||
      name === 'trash' ||
      name === 'spam' ||
      name === 'junk' ||
      name === 'papierkorb'
    )
  }

  /** True when a folder is the account's Junk / Spam mailbox —
      gates the "Clear Spam Folder" context-menu action (#483).
      Same attribute + name fallbacks as the junk branch of
      `standardRank`. */
  function isJunk(f: Folder): boolean {
    const name = f.name.toLowerCase()
    const attrs = f.attributes.map((a) => a.toLowerCase())
    const has = (k: string) => attrs.some((a) => a.includes(k))
    return has('junk') || has('spam') || name === 'spam' || name === 'junk'
  }

  /** Pick an icon for a folder. Resolution chain, highest priority
      first:
        1. Per-folder override set via the emoji picker — absolute
           winner so the user's explicit "I picked 📮 for my Inbox"
           beats the special-use default.
        2. IMAP special-use attributes (and a few name fallbacks)
           so INBOX/Sent/Drafts/etc. get canonical icons without
           the user having to pick anything.
        3. Keyword rules from Account.folder_icons (the older
           "folder name contains X → use Y" mechanism).
        4. Generic 📁 fallback.
     */
  /** Resolution result: either a stroke `Icon` we own (standard
   *  folder defaults) or a free-form glyph string the user
   *  picked (per-folder override or keyword rule) — those stay as
   *  emoji because that's the type their picker emits.  Callers
   *  branch on `kind` and render the matching surface. */
  type FolderGlyph =
    | { kind: 'icon'; name: IconName }
    | { kind: 'emoji'; value: string }

  function folderIcon(f: Folder): FolderGlyph {
    const account = accounts.find((a) => a.id === accountId)
    const override = account?.folder_icon_overrides?.[f.name]
    if (override) return { kind: 'emoji', value: override }

    const name = f.name.toLowerCase()
    const attrs = f.attributes.map((a) => a.toLowerCase())

    const has = (k: string) => attrs.some((a) => a.includes(k))
    if (name === 'inbox' || has('inbox')) return { kind: 'icon', name: 'global-inbox' }
    // #276 — synthetic local-only Outbox folder, rendered with
    // the same paper-airplane the Sent folder uses.  Outbox is
    // semantically "outgoing mail still in flight", which the
    // sent-mail glyph reads cleanly enough; per the project's
    // icon-reuse policy we don't add a dedicated SVG.
    if (f.name === OUTBOX_FOLDER) return { kind: 'icon', name: 'sent' }
    if (has('sent')) return { kind: 'icon', name: 'sent' }
    if (has('draft')) return { kind: 'icon', name: 'drafts' }
    if (has('trash') || has('deleted') || name === 'trash' || name === 'papierkorb')
      return { kind: 'icon', name: 'trash' }
    if (has('junk') || has('spam') || name === 'spam' || name === 'junk')
      return { kind: 'icon', name: 'spam' }
    if (has('flagged') || has('starred')) return { kind: 'icon', name: 'star' }
    if (has('archive')) return { kind: 'icon', name: 'archive' }
    // Some accounts ship a `Notes` folder (iCloud-backed accounts
    // expose one for inline notes saved from the iOS Notes app).
    // No standard IMAP attribute for it, so we match by name.
    if (name === 'notes' || name.endsWith('/notes')) return { kind: 'icon', name: 'notes' }

    // User-defined keyword rules win over the generic folder
    // fallback below, but they're picker-sourced emoji so they
    // stay rendered as strings.
    const rules = account?.folder_icons ?? []
    for (const rule of rules) {
      const kw = rule.keyword.trim().toLowerCase()
      if (kw && name.includes(kw)) return { kind: 'emoji', value: rule.icon }
    }

    return { kind: 'icon', name: 'files' }
  }

  /** True if an override is currently in effect for this folder —
   *  drives whether the picker's "Reset to default" button is
   *  enabled. */
  function hasIconOverride(f: Folder): boolean {
    const account = accounts.find((a) => a.id === accountId)
    return !!account?.folder_icon_overrides?.[f.name]
  }

  // Short display name: strip the hierarchy prefix so "INBOX/Work" shows
  // as "Work". INBOX itself keeps its name but title-cased.
  function displayName(f: Folder): string {
    if (f.name.toUpperCase() === 'INBOX') return 'Inbox'
    const delim = f.delimiter ?? '/'
    const parts = f.name.split(delim)
    return parts[parts.length - 1] || f.name
  }

  /** Rank each folder into the "standard" tier (Inbox / Drafts / Sent /
      Flagged / Archive / Junk / Trash) or the "user" tier. Standard
      folders get a numeric rank that drives the top-of-list order;
      user folders get -1 and are sorted alphabetically instead. The
      ordering mirrors what every major mail client shows — Inbox is
      where mail arrives, then the user's own outgoing queues, then
      the storage-ish folders at the bottom. */
  function standardRank(f: Folder): number {
    const name = f.name.toLowerCase()
    const attrs = f.attributes.map((a) => a.toLowerCase())
    const has = (k: string) => attrs.some((a) => a.includes(k))

    if (name === 'inbox' || has('inbox')) return 0
    // Outbox sits between Inbox and Drafts — outgoing traffic
    // ordering: where mail arrives → where it's queued to leave →
    // where it's parked mid-write → where it's already sent
    // (#276).  Recognised by the synthetic-folder name; never
    // matches a real IMAP folder because the sidebar only uses
    // this name for the local-only injection.
    if (f.name === OUTBOX_FOLDER) return 0.5
    if (has('draft')) return 1
    if (has('sent')) return 2
    if (has('flagged') || has('starred')) return 3
    if (has('archive')) return 4
    if (
      has('junk') ||
      has('spam') ||
      name === 'spam' ||
      name === 'junk'
    )
      return 5
    if (
      has('trash') ||
      has('deleted') ||
      name === 'trash' ||
      name === 'papierkorb'
    )
      return 6
    return -1
  }

  // Split the flat server-returned list into the two tiers so the
  // template renders them in distinct `{#each}` blocks with a
  // divider in between. `$derived` so the sort work only re-runs when
  // `folders` actually changes.
  /** Inject a synthetic local-only "Outbox" folder when the
   *  global queue is non-empty.  Lives at rank 0.5 (between
   *  Inbox and Drafts).  No IMAP attributes — the
   *  Outbox-specific UI in MailList branches on `folder ===
   *  OUTBOX_FOLDER` rather than reading these. */
  const outboxSynthetic: Folder | null = $derived(
    outboxCount > 0
      ? {
          name: OUTBOX_FOLDER,
          delimiter: null,
          attributes: [],
          unread_count: outboxCount,
        }
      : null,
  )

  const standardFolders = $derived(
    [
      ...(outboxSynthetic ? [outboxSynthetic] : []),
      ...folders.filter((f) => standardRank(f) !== -1),
    ].sort((a, b) => standardRank(a) - standardRank(b)),
  )

  /** Sibling order at every tree level. `localeCompare` so non-ASCII
   *  folder names (Entwürfe, Übersicht…) sort the way the user's
   *  locale expects instead of by code point. */
  function compareFolders(a: Folder, b: Folder): number {
    return displayName(a).localeCompare(displayName(b), undefined, {
      sensitivity: 'base',
      numeric: true,
    })
  }

  /** The two render tiers, flattened depth-first so subfolders sit
   *  directly under their parent and indent one step per level
   *  (#478). Standard folders stay in canonical rank order; their
   *  user subfolders (e.g. `INBOX/Work`) indent underneath them
   *  rather than drifting into the alphabetical custom tier. */
  const folderRows = $derived(
    buildFolderRows(
      folders,
      standardFolders,
      (f) => standardRank(f) !== -1,
      compareFolders,
    ),
  )
</script>

<aside
  class="shrink-0 border-r glass-panel flex flex-col"
  use:resizableSidebar={{ key: 'mail.folderSidebar', defaultWidth: 224, min: 160, max: 480 }}
>
  <!-- Compose CTA. Emoji makes the primary action visually anchored —
       matches Nick's ask for "nice emoji" on the button. -->
  <div class="p-3">
    <button class="btn preset-filled-primary-500 w-full inline-flex items-center justify-center gap-1.5" data-tour="compose" onclick={() => oncompose?.()}>
      <Icon name="compose" size={16} /> Compose
    </button>
  </div>

  <!-- Folder tree. Takes every vertical pixel below the Compose
       button now that the refresh / unified toggle / integration
       nav / settings slot have all moved out of this component.
       Folder-management (new / rename / delete) is surfaced via a
       subtle header "+" for top-level creates and a right-click
       context menu on each row for subfolder / rename / delete. -->

  <!-- `depth` indents subfolder rows one icon-width per nesting
       level (#478). The indent is padding, not margin, so the
       hover / selected / drag-highlight fill still spans the full
       row width and nested rows read as part of the same list. -->
  {#snippet folderRow(folder: Folder, depth: number)}
    {#if renamingFolder === folder.name}
      <!-- Inline rename. `bind:this` + `autofocus` on the input
           is set from the `$effect` on `renamingFolder` below so
           the caret lands in the field the moment the menu's
           "Rename" click settles. Escape bails, Enter commits,
           blur also commits (matches most file managers). -->
      {@const glyph = folderIcon(folder)}
      <div
        class="flex items-center gap-2 pr-3 py-1.5"
        style:padding-left={`${0.75 + depth * 1}rem`}
      >
        {#if glyph.kind === 'icon'}
          <Icon name={glyph.name} size={16} />
        {:else}
          <span>{glyph.value}</span>
        {/if}
        <!-- svelte-ignore a11y_autofocus -->
        <input
          type="text"
          class="input flex-1 text-sm px-2 py-1 rounded-lg"
          bind:value={renameValue}
          disabled={folderOpBusy}
          autofocus
          onkeydown={(e) => {
            if (e.key === 'Enter') { e.preventDefault(); void commitRename() }
            else if (e.key === 'Escape') { e.preventDefault(); cancelRename() }
          }}
          onblur={() => { if (renamingFolder) void commitRename() }}
        />
      </div>
    {:else}
      {@const glyph = folderIcon(folder)}
      <div
        role="button"
        tabindex="0"
        class="group w-full flex items-center gap-2 pr-3 py-2 rounded-lg text-sm cursor-pointer transition-colors duration-150 ease-out
          {selectedFolder === folder.name
            ? 'bg-primary-500/12 text-primary-500 font-medium ring-1 ring-inset ring-primary-500/30'
            : dragOverFolder === folder.name
              ? 'bg-primary-500/20 ring-2 ring-primary-500'
              : 'hover:bg-primary-500/10'}"
        style:padding-left={`${0.75 + depth * 1}rem`}
        onclick={() => onselectfolder(folder.name)}
        onkeydown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault()
            onselectfolder(folder.name)
          }
        }}
        oncontextmenu={(e) => openContextMenu(e, folder)}
        ondragover={(e) => onFolderDragOver(e, folder)}
        ondragleave={() => onFolderDragLeave(folder)}
        ondrop={(e) => void onFolderDrop(e, folder)}
      >
        {#if glyph.kind === 'icon'}
          <Icon name={glyph.name} size={16} />
        {:else}
          <span>{glyph.value}</span>
        {/if}
        <span class="flex-1 text-left truncate">{displayName(folder)}</span>
        {#if folder.unread_count && folder.unread_count > 0 && !isTrashOrJunk(folder)}
          <span class="badge preset-filled-primary-500 text-xs">{folder.unread_count}</span>
        {/if}
        <!-- Three-dot trigger.  Mirrors the right-click menu so
             trackpad-only / touchscreen users have the same
             affordances.  Surfaces on hover or whenever this
             folder's menu is the open one. -->
        <button
          class="w-5 h-5 rounded-lg text-surface-500 hover:bg-primary-500/10 transition-colors duration-150 ease-out leading-none shrink-0
                 {contextMenu?.folder.name === folder.name
                   ? 'opacity-100'
                   : 'opacity-0 group-hover:opacity-100 focus:opacity-100'}"
          title="More actions"
          aria-label="Folder actions"
          onclick={(e) => {
            e.stopPropagation()
            const r = anchorRect(e.currentTarget as HTMLElement)
            // Reuse the same `contextMenu` state that the
            // right-click handler populates so both surfaces
            // share one menu component.
            contextMenu = { folder, ...clampToViewport({ x: r.right + 4, y: r.top }, 200, 220) }
          }}
        >⋯</button>
      </div>
    {/if}
  {/snippet}

  <!-- Subtle header. "Folders" label + a `+` for adding a new
       top-level folder. Hidden in unified mode because a top-level
       folder would land on one account but the user's looking at
       all of them at once. -->
  {#if !unified}
    <div class="flex items-center justify-between px-3 pt-2 pb-1">
      <span class="text-[10px] font-semibold text-surface-500 uppercase tracking-wider">
        Folders
      </span>
      <button
        class="w-5 h-5 rounded-lg flex items-center justify-center text-surface-500 hover:bg-primary-500/10 transition-colors duration-150 ease-out disabled:opacity-50"
        title="New folder"
        aria-label="New folder"
        disabled={folderOpBusy}
        onclick={() => {
          renamingFolder = null
          contextMenu = null
          newFolderInput = { parent: null, value: '' }
        }}
      >+</button>
    </div>
  {/if}

  <!-- `py-1` reserves space inside the scroll container so the
       drag-hover `ring-2` on the topmost folder row (typically
       Inbox) doesn't get clipped where its outer stroke would
       otherwise sit at the very top edge of `overflow-y-auto`.
       `space-y-0.5` keeps a hairline gap between rows — the hover
       and selected fills are the same translucent primary tint, so
       flush rows would visually merge into one blob when the row
       adjacent to the selected one is hovered (#465). -->
  <nav class="flex-1 overflow-y-auto px-2 py-1 space-y-0.5">
    {#if unified}
      <!-- Unified mode surfaces three global views: All Inboxes
           (existing), All Sent (#322), All Drafts (#322).  Sent and
           Drafts can't reuse the literal IMAP folder name the way
           Inbox does — see `unifiedFolders.ts` — so we route them
           through sentinel folder names that MailList recognises and
           dispatches to the per-account-resolving backend commands. -->
      <button
        class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors duration-150 ease-out
          {selectedFolder === 'INBOX'
            ? 'bg-primary-500/12 text-primary-500 font-medium ring-1 ring-inset ring-primary-500/30'
            : 'hover:bg-primary-500/10'}"
        onclick={() => onselectfolder('INBOX')}
      >
        <span
          class="inline-flex items-center justify-center w-5 h-5 shrink-0"
          aria-hidden="true"
        >
          <Icon name="global-inbox" size={16} />
        </span>
        <span class="flex-1 text-left truncate">{m.sidebar_unified_inbox_label()}</span>
        {#if unifiedUnread > 0}
          <span class="badge preset-filled-primary-500 text-xs">{unifiedUnread}</span>
        {/if}
      </button>
      {#if outboxCount > 0}
        <!-- Global Outbox (#322 follow-up): only rendered when at
             least one queued message exists across all accounts —
             matches the per-account synthetic Outbox's "hide when
             empty" behaviour, so a healthy install with nothing
             queued sees the same three-button list as before.
             Routing reuses the existing OUTBOX_FOLDER channel: when
             selected with `unifiedMode = true`, App.svelte mounts
             OutboxList with `unified={true}`, which already calls
             `list_all_outbox` to merge every account's queue. -->
        <button
          class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors duration-150 ease-out
            {selectedFolder === OUTBOX_FOLDER
              ? 'bg-primary-500/12 text-primary-500 font-medium ring-1 ring-inset ring-primary-500/30'
              : 'hover:bg-primary-500/10'}"
          onclick={() => onselectfolder(OUTBOX_FOLDER)}
        >
          <span
            class="inline-flex items-center justify-center w-5 h-5 shrink-0"
            aria-hidden="true"
          >
            <Icon name="sent" size={16} />
          </span>
          <span class="flex-1 text-left truncate">{m.sidebar_unified_outbox_label()}</span>
          <span class="badge preset-filled-primary-500 text-xs">{outboxCount}</span>
        </button>
      {/if}
      <!-- Order mirrors the per-account standard-folder ranking
           (`standardRank` below): Inbox → Outbox → Drafts → Sent →
           Archive → Junk → Trash. Same shape on both surfaces so a
           user toggling between unified and a single account never
           sees their outgoing-mail folders shuffle. -->
      <button
        class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors duration-150 ease-out
          {selectedFolder === UNIFIED_DRAFTS_FOLDER
            ? 'bg-primary-500/12 text-primary-500 font-medium ring-1 ring-inset ring-primary-500/30'
            : 'hover:bg-primary-500/10'}"
        onclick={() => onselectfolder(UNIFIED_DRAFTS_FOLDER)}
      >
        <span
          class="inline-flex items-center justify-center w-5 h-5 shrink-0"
          aria-hidden="true"
        >
          <Icon name="drafts" size={16} />
        </span>
        <span class="flex-1 text-left truncate">{m.sidebar_unified_drafts_label()}</span>
      </button>
      <button
        class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors duration-150 ease-out
          {selectedFolder === UNIFIED_SENT_FOLDER
            ? 'bg-primary-500/12 text-primary-500 font-medium ring-1 ring-inset ring-primary-500/30'
            : 'hover:bg-primary-500/10'}"
        onclick={() => onselectfolder(UNIFIED_SENT_FOLDER)}
      >
        <span
          class="inline-flex items-center justify-center w-5 h-5 shrink-0"
          aria-hidden="true"
        >
          <Icon name="sent" size={16} />
        </span>
        <span class="flex-1 text-left truncate">{m.sidebar_unified_sent_label()}</span>
      </button>
      <button
        class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors duration-150 ease-out
          {selectedFolder === UNIFIED_ARCHIVE_FOLDER
            ? 'bg-primary-500/12 text-primary-500 font-medium ring-1 ring-inset ring-primary-500/30'
            : 'hover:bg-primary-500/10'}"
        onclick={() => onselectfolder(UNIFIED_ARCHIVE_FOLDER)}
      >
        <span
          class="inline-flex items-center justify-center w-5 h-5 shrink-0"
          aria-hidden="true"
        >
          <Icon name="archive" size={16} />
        </span>
        <span class="flex-1 text-left truncate">{m.sidebar_unified_archive_label()}</span>
      </button>
      <button
        class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors duration-150 ease-out
          {selectedFolder === UNIFIED_JUNK_FOLDER
            ? 'bg-primary-500/12 text-primary-500 font-medium ring-1 ring-inset ring-primary-500/30'
            : 'hover:bg-primary-500/10'}"
        onclick={() => onselectfolder(UNIFIED_JUNK_FOLDER)}
      >
        <span
          class="inline-flex items-center justify-center w-5 h-5 shrink-0"
          aria-hidden="true"
        >
          <Icon name="spam" size={16} />
        </span>
        <span class="flex-1 text-left truncate">{m.sidebar_unified_junk_label()}</span>
      </button>
      <button
        class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm transition-colors duration-150 ease-out
          {selectedFolder === UNIFIED_TRASH_FOLDER
            ? 'bg-primary-500/12 text-primary-500 font-medium ring-1 ring-inset ring-primary-500/30'
            : 'hover:bg-primary-500/10'}"
        onclick={() => onselectfolder(UNIFIED_TRASH_FOLDER)}
      >
        <span
          class="inline-flex items-center justify-center w-5 h-5 shrink-0"
          aria-hidden="true"
        >
          <Icon name="trash" size={16} />
        </span>
        <span class="flex-1 text-left truncate">{m.sidebar_unified_trash_label()}</span>
      </button>
    {:else if loading}
      <p class="px-3 py-2 text-xs text-surface-500">Loading folders…</p>
    {:else if error}
      <p class="px-3 py-2 text-xs text-red-500">{error}</p>
    {:else if folders.length === 0}
      <p class="px-3 py-2 text-xs text-surface-500">No folders.</p>
    {:else}
      {#each folderRows.standard as row (row.folder.name)}
        {@render folderRow(row.folder, row.depth)}
      {/each}

      {#if folderRows.standard.length > 0 && folderRows.custom.length > 0}
        <hr class="my-2 mx-2 border-surface-200 dark:border-surface-700" />
      {/if}

      {#each folderRows.custom as row (row.folder.name)}
        {@render folderRow(row.folder, row.depth)}
      {/each}

      <!-- New-folder inline input. Appears at the bottom of the
           folder list regardless of whether it's a top-level or
           subfolder create — the `parent` label makes the context
           clear without routing the input into the middle of the
           tree (which would be nice but gets fiddly with the
           two-tier standard/custom split). -->
      {#if newFolderInput}
        <div class="flex items-center gap-2 px-3 py-1.5 mt-1">
          <Icon name="files" size={16} />
          <!-- svelte-ignore a11y_autofocus -->
          <input
            type="text"
            class="input flex-1 text-sm px-2 py-1 rounded-lg"
            placeholder={newFolderInput.parent
              ? `New subfolder in ${displayNameFromPath(newFolderInput.parent)}`
              : 'New folder'}
            bind:value={newFolderInput.value}
            disabled={folderOpBusy}
            autofocus
            onkeydown={(e) => {
              if (e.key === 'Enter') { e.preventDefault(); void commitNewFolder() }
              else if (e.key === 'Escape') { e.preventDefault(); cancelNewFolder() }
            }}
            onblur={() => {
              // Commit on blur only if there's actually text —
              // tabbing away from an empty input should just close.
              if (newFolderInput && newFolderInput.value.trim()) {
                void commitNewFolder()
              } else {
                cancelNewFolder()
              }
            }}
          />
        </div>
      {/if}

      <!-- Non-blocking feedback for the last folder-management
           operation's error. Clears when the next menu opens or the
           user starts a new operation. -->
      {#if folderOpError}
        <p class="px-3 py-1.5 mt-1 text-xs text-red-500 wrap-break-word">{folderOpError}</p>
      {/if}
    {/if}
  </nav>
</aside>

<!-- Right-click context menu. `position: fixed` anchored at the
     click point; z-60 to clear the IconRail (z-ordering of the
     sidebar's `aside`). Rename / Delete are disabled for
     special-use folders — most servers refuse to rename or delete
     the canonical Inbox / Sent / Drafts / etc., and even when they
     don't the account's special-use attributes then point at a
     folder that no longer exists, which breaks `pick_*_folder`
     resolution in save_draft / archive / trash flows. -->
{#if contextMenu}
  {@const stdFolder = standardRank(contextMenu.folder) !== -1}
  {@const outboxRow = contextMenu.folder.name === OUTBOX_FOLDER}
  <div
    class="fixed z-60 min-w-44 rounded-xl glass-float py-1 text-sm"
    style="left: {contextMenu.x}px; top: {contextMenu.y}px;"
    role="menu"
    tabindex="-1"
    onmousedown={(e) => e.stopPropagation()}
  >
    <button
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-primary-500/10 transition-colors duration-150 ease-out disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={folderOpBusy}
      onclick={() => {
        const parent = contextMenu!.folder.name
        contextMenu = null
        newFolderInput = { parent, value: '' }
      }}
    >
      <Icon name="add-folder" size={16} />
      <span>New subfolder</span>
    </button>
    <button
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-primary-500/10 transition-colors duration-150 ease-out disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={folderOpBusy}
      onclick={() => {
        const f = contextMenu!.folder
        contextMenu = null
        openIconPicker(f)
      }}
    >
      <Icon name="emoji" size={16} />
      <span>Change icon…</span>
    </button>
    <!-- #483: whole-folder read toggle. Hidden for the synthetic
         local-only Outbox — its rows have no `\Seen` flag to set. -->
    {#if !outboxRow}
      <button
        class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-primary-500/10 transition-colors duration-150 ease-out disabled:opacity-50 disabled:hover:bg-transparent"
        disabled={folderOpBusy}
        onclick={() => {
          const f = contextMenu!.folder
          contextMenu = null
          void markAllRead(f)
        }}
      >
        <Icon name="read" size={16} />
        <span>{m.sidebar_menu_mark_all_read()}</span>
      </button>
    {/if}
    <button
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-primary-500/10 transition-colors duration-150 ease-out disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={folderOpBusy || stdFolder}
      title={stdFolder ? "Standard folders can't be renamed" : ''}
      onclick={() => {
        const f = contextMenu!.folder
        contextMenu = null
        renamingFolder = f.name
        renameValue = displayName(f)
      }}
    >
      <Icon name="compose" size={16} />
      <span>Rename</span>
    </button>
    <!-- #483: bulk-empty the spam folder. Only offered on the
         account's Junk mailbox — it's the one folder where "throw
         everything away unread" is the normal gesture. -->
    {#if isJunk(contextMenu.folder)}
      <button
        class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-red-500/10 transition-colors duration-150 ease-out text-red-600 dark:text-red-400 disabled:opacity-50 disabled:hover:bg-transparent"
        disabled={folderOpBusy}
        onclick={() => {
          const f = contextMenu!.folder
          contextMenu = null
          clearSpamConfirm = { folder: f }
        }}
      >
        <Icon name="trash" size={16} />
        <span>{m.sidebar_menu_clear_spam()}</span>
      </button>
    {/if}
    <button
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-red-500/10 transition-colors duration-150 ease-out text-red-600 dark:text-red-400 disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={folderOpBusy || stdFolder}
      title={stdFolder ? "Standard folders can't be deleted" : ''}
      onclick={() => {
        const f = contextMenu!.folder
        contextMenu = null
        deleteConfirm = { folder: f }
      }}
    >
      <Icon name="delete-folder" size={16} />
      <span>Delete</span>
    </button>
  </div>
{/if}

<!-- Delete confirmation modal. Destructive ops always pass through
     an explicit confirm — IMAP DELETE usually refuses non-empty
     folders but a freshly-created / emptied one disappears without
     a peep, and rebuilding it isn't possible if it carried custom
     subfolders. -->
{#if deleteConfirm}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) cancelDelete() }}
  >
    <div class="glass-float rounded-2xl w-96 max-w-full p-5">
      <h3 class="text-base font-semibold mb-2">Delete folder?</h3>
      <p class="text-sm text-surface-700 dark:text-surface-300 mb-4">
        Delete <span class="font-medium">{displayName(deleteConfirm.folder)}</span>?
        This can't be undone.
      </p>
      {#if folderOpError}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{folderOpError}</p>
      {/if}
      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500"
          disabled={folderOpBusy}
          onclick={cancelDelete}
        >Cancel</button>
        <button
          class="btn preset-filled-error-500"
          disabled={folderOpBusy}
          onclick={() => void confirmDelete()}
        >{folderOpBusy ? 'Deleting…' : 'Delete'}</button>
      </div>
    </div>
  </div>
{/if}

<!-- Clear-spam confirmation modal (#483). Same shell as the
     delete-folder confirm above — this one destroys the folder's
     *messages* (permanently, no trash detour), not the folder
     itself, so it gets its own copy. -->
{#if clearSpamConfirm}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) cancelClearSpam() }}
  >
    <div class="glass-float rounded-2xl w-96 max-w-full p-5">
      <h3 class="text-base font-semibold mb-2">{m.sidebar_clear_spam_title()}</h3>
      <p class="text-sm text-surface-700 dark:text-surface-300 mb-4">
        {m.sidebar_clear_spam_body({ folder: displayName(clearSpamConfirm.folder) })}
      </p>
      {#if folderOpError}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{folderOpError}</p>
      {/if}
      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500"
          disabled={folderOpBusy}
          onclick={cancelClearSpam}
        >{m.sidebar_clear_spam_cancel()}</button>
        <button
          class="btn preset-filled-error-500"
          disabled={folderOpBusy}
          onclick={() => void confirmClearSpam()}
        >{folderOpBusy ? m.sidebar_clear_spam_confirming() : m.sidebar_clear_spam_confirm()}</button>
      </div>
    </div>
  </div>
{/if}

<!-- "Change icon" modal — uses the shared EmojiPicker component.
     Picking any emoji commits immediately; the ∅ "no emoji"
     tile clears the override and falls back to the default
     resolution chain (special-use → keyword rule → 📁). -->
{#if iconPicker}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) closeIconPicker() }}
  >
    <div class="glass-float rounded-2xl max-w-full p-5">
      <h3 class="text-base font-semibold mb-1">Choose an icon</h3>
      <p class="text-xs text-surface-500 mb-4">
        For <span class="font-medium text-surface-700 dark:text-surface-300">{displayName(iconPicker.folder)}</span>
      </p>

      <EmojiPicker
        value={hasIconOverride(iconPicker.folder)
          ? (accounts.find((a) => a.id === accountId)?.folder_icon_overrides?.[iconPicker.folder.name] ?? null)
          : null}
        onpick={(emoji) => void commitFolderIcon(iconPicker!.folder, emoji)}
      />

      {#if folderOpError}
        <p class="text-xs text-red-500 mt-3 wrap-break-word">{folderOpError}</p>
      {/if}

      <div class="flex justify-end mt-3">
        <button
          class="btn preset-outlined-surface-500"
          disabled={folderOpBusy}
          onclick={closeIconPicker}
        >Cancel</button>
      </div>
    </div>
  </div>
{/if}

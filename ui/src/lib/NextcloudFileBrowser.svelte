<script lang="ts">
  /**
   * NextcloudFileBrowser — the inner "browse Nextcloud" UI shared by
   * the modal picker (`NextcloudFilePicker`) and the sidebar-routed
   * full-pane view (`FilesView`).
   *
   * Owns the *browse* concerns and nothing else:
   *   - account loading + selection (only shown when N > 1)
   *   - folder navigation + breadcrumbs
   *   - listing + sort (folders first, natural sort within each group)
   *   - multi-select (unless `pickFolderMode` hides the checkboxes)
   *   - inline "+ New folder" with validation
   *
   * Action buttons (Attach / Share / Send via mail / …) are rendered
   * by the parent — the parent is the only one that knows what should
   * happen when the user commits a selection. We expose enough state
   * via `bind:`-able props for that footer to be smart.
   *
   * # Bindable state
   *
   *   bind:accountId    — currently active account id ('' = none)
   *   bind:currentPath  — folder we're currently looking at ('/' = root)
   *   bind:selected     — Set of paths the user has ticked
   *   bind:entries      — current folder's children (for content_type lookup)
   *   bind:accounts     — list of NC accounts (so the parent can detect
   *                       "no accounts connected" without re-fetching)
   *   bind:error        — last error string ('' = none) — parent renders
   *                       it wherever it wants
   */

  import { invoke } from '@tauri-apps/api/core'
  import { formatError } from './errors'
  import Icon, { type IconName } from './Icon.svelte'
  import FileTypeIcon from './FileTypeIcon.svelte'
  import NcPreview from './NcPreview.svelte'
  import { m } from '../paraglide/messages'

  interface NextcloudCapabilities {
    version?: string | null
    talk: boolean
    files: boolean
    caldav: boolean
    carddav: boolean
  }
  export interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
    capabilities?: NextcloudCapabilities | null
  }
  export interface FileEntry {
    name: string
    path: string
    is_dir: boolean
    size: number | null
    content_type: string | null
    modified: string | null
  }

  interface Props {
    /** Hide checkboxes — used by the picker's "save here" mode where
        the user is choosing a destination folder, not files. */
    pickFolderMode?: boolean
    accountId?: string
    currentPath?: string
    selected?: Set<string>
    /** Subset of `selected` whose paths are folders.  Tracked
     *  alongside `selected` so the picker can distinguish file
     *  vs. folder selections even when the user has navigated
     *  away from the folder where they were ticked.  Without
     *  this, the file/folder count derives only from the
     *  currently-visible `entries` list and lies whenever the
     *  user changes folders mid-selection. */
    selectedDirs?: Set<string>
    entries?: FileEntry[]
    accounts?: NextcloudAccount[]
    error?: string
    /** Optional case-insensitive substring filter applied to the
     *  rendered list only.  `bind:entries` still exposes the full
     *  unfiltered folder listing so the parent's action footer
     *  (file/folder counts, content_type lookup) doesn't have to
     *  care about whether the user is searching. */
    filterQuery?: string
  }
  let {
    pickFolderMode = false,
    accountId = $bindable(''),
    currentPath = $bindable('/'),
    selected = $bindable(new Set<string>()),
    selectedDirs = $bindable(new Set<string>()),
    entries = $bindable<FileEntry[]>([]),
    accounts = $bindable<NextcloudAccount[]>([]),
    error = $bindable(''),
    filterQuery = '',
  }: Props = $props()

  /** Filtered view of `entries` for rendering.  Matches by basename
   *  case-insensitively; folders match the same way so the user can
   *  drill into a subfolder by name without leaving the search.
   *  Empty query short-circuits to the full list. */
  const visibleEntries = $derived.by(() => {
    const q = filterQuery.trim().toLowerCase()
    if (!q) return entries
    return entries.filter((e) => e.name.toLowerCase().includes(q))
  })

  let loading = $state(false)
  /** Per-`${accountId}::${path}` cache of folder listings.
   *  Re-visiting a folder during the same picker session
   *  (very common while ticking files across folders) is now
   *  synchronous — no blank flash, no IPC.  Stale data is
   *  refreshed in the background so the user sees additions
   *  / removals without having to manually reload. */
  const FOLDER_CACHE = new Map<string, FileEntry[]>()
  function folderCacheKey(p: string): string {
    return `${accountId}::${p}`
  }
  function sortEntries(list: FileEntry[]) {
    // Folders first, then files.  Within each group, natural
    // sort so numbered names (1, 2, 9, 10, 11) come out the
    // way humans expect — matches the NC web UI ordering.
    list.sort((a, b) => {
      if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1
      return a.name.localeCompare(b.name, undefined, {
        numeric: true,
        sensitivity: 'base',
      })
    })
  }
  // "+ New folder" inline input — hidden until the user clicks the
  // button. Driven by component state so we can validate and show
  // errors inline rather than via a blocking `prompt()`.
  let creatingFolder = $state(false)
  let newFolderName = $state('')
  let creatingFolderInFlight = $state(false)
  let newFolderInput = $state<HTMLInputElement | null>(null)
  $effect(() => {
    if (creatingFolder && newFolderInput) {
      newFolderInput.focus()
    }
  })

  $effect(() => {
    loadAccounts()
  })

  async function loadAccounts() {
    try {
      const list = await invoke<NextcloudAccount[]>('get_nextcloud_accounts')
      accounts = list
      if (list.length === 1 && !accountId) {
        accountId = list[0].id
        await loadFolder('/')
      }
    } catch (e) {
      error = formatError(e) || 'Failed to load Nextcloud accounts'
    }
  }

  async function selectAccount(id: string) {
    accountId = id
    selected = new Set()
    selectedDirs = new Set()
    await loadFolder('/')
  }

  async function loadFolder(path: string) {
    if (!accountId) return
    error = ''
    const cacheKey = folderCacheKey(path)
    const cached = FOLDER_CACHE.get(cacheKey)
    if (cached) {
      // Cached — paint immediately, refresh in the background.
      // Re-visiting a folder during the same picker session is
      // synchronous, no blank flash and no spinner.  If the
      // server-side listing has changed since the last visit
      // the background refresh below silently updates entries.
      entries = cached
      currentPath = path
      loading = false
    } else {
      // First visit — clear entries so the breadcrumb and the
      // list stay in sync.  Showing the previous folder's
      // entries under a new breadcrumb path is more confusing
      // than a brief loading state.
      entries = []
      currentPath = path
      loading = true
    }
    try {
      const list = await invoke<FileEntry[]>('list_nextcloud_files', {
        ncId: accountId,
        path,
      })
      sortEntries(list)
      FOLDER_CACHE.set(cacheKey, list)
      // Race-guard: the user may have navigated again before
      // this refresh returned.
      if (currentPath === path) entries = list
    } catch (e) {
      error = formatError(e) || 'Failed to list folder'
    } finally {
      loading = false
    }
  }

  // Breadcrumb segments — each one is a clickable jump back up the
  // tree.
  let breadcrumbs = $derived.by(() => {
    const segs = currentPath.split('/').filter(Boolean)
    const out = [{ label: 'Home', path: '/' }]
    let acc = ''
    for (const s of segs) {
      acc += '/' + s
      out.push({ label: s, path: acc })
    }
    return out
  })

  function onEntryClick(entry: FileEntry) {
    if (entry.is_dir) {
      loadFolder(entry.path)
    } else if (!pickFolderMode) {
      // In folder-pick mode files aren't selectable — they're shown
      // only as a preview of what's already in the destination folder.
      toggleSelected(entry.path, entry.is_dir)
    }
  }

  function toggleSelected(path: string, isDir: boolean) {
    // Svelte 5 Sets need reassignment to trigger reactivity.
    const next = new Set(selected)
    const nextDirs = new Set(selectedDirs)
    if (next.has(path)) {
      next.delete(path)
      nextDirs.delete(path)
    } else {
      next.add(path)
      if (isDir) nextDirs.add(path)
    }
    selected = next
    selectedDirs = nextDirs
  }

  function formatSize(bytes: number | null): string {
    if (bytes == null) return ''
    if (bytes < 1024) return `${bytes} B`
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`
    return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`
  }

  /** Stroke-icon for entries that read better as a glyph than
   *  as a typed `FileTypeIcon` SVG (folders + plaintext).
   *  Documents (images, markdown, office, archives) fall
   *  through to `null` so the row renders FileTypeIcon
   *  instead. */
  function iconNameFor(entry: FileEntry): IconName | null {
    if (entry.is_dir) return 'files'
    const ct = entry.content_type ?? ''
    const fn = entry.name.toLowerCase()
    if (ct.startsWith('text/') && !ct.includes('markdown') && !fn.endsWith('.md') && !fn.endsWith('.markdown'))
      return 'notes'
    return null
  }

  function navigateTo(path: string) {
    loadFolder(path)
  }

  function startCreateFolder() {
    newFolderName = ''
    error = ''
    creatingFolder = true
  }

  function cancelCreateFolder() {
    creatingFolder = false
    newFolderName = ''
  }

  /**
   * Create the folder named in `newFolderName` inside `currentPath`,
   * refresh the listing, and pre-select the new folder so the parent's
   * "Share as link" / "Send via mail" footer button immediately
   * targets it.
   */
  async function confirmCreateFolder() {
    const trimmed = newFolderName.trim()
    if (!trimmed) {
      error = 'Folder name is required.'
      return
    }
    // Names with path separators would silently change which folder
    // the create lands in — block here rather than let the server
    // return a confusing 409.
    if (trimmed.includes('/') || trimmed.includes('\\')) {
      error = "Folder name can't contain '/' or '\\'."
      return
    }
    creatingFolderInFlight = true
    error = ''
    try {
      const base = currentPath.endsWith('/') ? currentPath : `${currentPath}/`
      const fullPath = `${base}${trimmed}`
      await invoke('create_nextcloud_directory', {
        ncId: accountId,
        path: fullPath,
      })
      creatingFolder = false
      newFolderName = ''
      // Invalidate the parent's cached listing so loadFolder
      // re-fetches and shows the new directory.
      FOLDER_CACHE.delete(folderCacheKey(currentPath))
      await loadFolder(currentPath)
      const folderPath = `${fullPath}/`
      const next = new Set(selected)
      next.add(folderPath)
      selected = next
    } catch (e) {
      error = formatError(e) || 'Failed to create folder'
    } finally {
      creatingFolderInFlight = false
    }
  }
</script>

{#if accounts.length === 0}
  <div class="p-6 text-sm text-surface-500">
    No Nextcloud account connected. Add one under
    <strong>Settings → Nextcloud</strong> first.
  </div>
{:else}
  {#if accounts.length > 1}
    <div class="px-5 py-2 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
      <label for="nc-files-account" class="text-xs text-surface-500">Account</label>
      <select
        id="nc-files-account"
        class="select text-sm"
        value={accountId}
        onchange={(e) => selectAccount((e.target as HTMLSelectElement).value)}
      >
        <option value="" disabled>Choose an account</option>
        {#each accounts as acc (acc.id)}
          <option value={acc.id}>{acc.display_name ?? acc.username} ({acc.server_url})</option>
        {/each}
      </select>
    </div>
  {/if}

  {#if accountId}
    <nav class="px-5 py-2 border-b border-surface-200 dark:border-surface-700 flex items-center gap-1 text-xs overflow-x-auto">
      {#each breadcrumbs as crumb, i (crumb.path)}
        {#if i > 0}<span class="text-surface-400">/</span>{/if}
        <button
          class="hover:underline {i === breadcrumbs.length - 1 ? 'font-semibold' : 'text-primary-500'}"
          onclick={() => navigateTo(crumb.path)}
          disabled={i === breadcrumbs.length - 1}
        >{crumb.label}</button>
      {/each}
      <div class="flex-1"></div>
      {#if !creatingFolder}
        <button
          class="text-primary-500 hover:bg-primary-500/10 rounded-md p-1 inline-flex items-center justify-center"
          onclick={startCreateFolder}
          title="Create a new folder in this directory"
          aria-label="New folder"
        ><Icon name="add-folder" size={16} /></button>
      {/if}
    </nav>

    {#if creatingFolder}
      <!--
        Inline name input rendered as its own row so a long folder name
        doesn't squeeze the breadcrumbs off-screen on narrow widths.
      -->
      <div class="px-5 py-2 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
        <label for="nc-new-folder" class="text-xs text-surface-500">New folder name</label>
        <input
          id="nc-new-folder"
          class="input flex-1 px-3 py-1.5 text-sm rounded-md"
          bind:this={newFolderInput}
          bind:value={newFolderName}
          placeholder="My folder"
          onkeydown={(e) => {
            if (e.key === 'Enter') confirmCreateFolder()
            else if (e.key === 'Escape') cancelCreateFolder()
          }}
        />
        <button
          class="btn preset-filled-primary-500 text-xs"
          disabled={creatingFolderInFlight || !newFolderName.trim()}
          onclick={confirmCreateFolder}
        >{creatingFolderInFlight ? 'Creating…' : 'Create'}</button>
        <button
          class="btn preset-outlined-surface-500 text-xs"
          disabled={creatingFolderInFlight}
          onclick={cancelCreateFolder}
        >Cancel</button>
      </div>
    {/if}

    <div class="flex-1 overflow-y-auto">
      {#if loading}
        <div class="p-6 text-sm text-surface-500">Loading…</div>
      {:else if entries.length === 0}
        <div class="p-6 text-sm text-surface-500">This folder is empty.</div>
      {:else if visibleEntries.length === 0}
        <!-- Folder has files but the filter narrowed everything to
             zero — distinct empty state so the user understands
             they've searched themselves out of results rather than
             landing in an empty folder. -->
        <div class="p-6 text-sm text-surface-500">{m.files_view_no_matches()}</div>
      {:else}
        <ul class="divide-y divide-surface-200 dark:divide-surface-800">
          {#each visibleEntries as entry (entry.path)}
            {@const iconName = iconNameFor(entry)}
            <li>
              <!-- Outer row uses role="button" instead of an actual
                   `<button>` because it nests an inner role="checkbox"
                   button — buttons can't nest in HTML, and the linter
                   warns about the resulting invalid SSR markup
                   (svelte/e/node_invalid_placement_ssr).  Same click +
                   keyboard semantics, valid markup. -->
              <div
                role="button"
                tabindex="0"
                class="w-full flex items-center gap-3 px-5 py-2 text-left cursor-pointer hover:bg-surface-100 dark:hover:bg-surface-800"
                onclick={() => onEntryClick(entry)}
                onkeydown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault()
                    onEntryClick(entry)
                  }
                }}
              >
                <!--
                  Folders also get a checkbox so the user can select a
                  folder to share as a public link. Click stops
                  propagation so the row's onclick (navigate-into) doesn't
                  also fire.
                -->
                {#if !pickFolderMode}
                  {@const isChecked = selected.has(entry.path)}
                  <!-- Compact circular toggle (#160).  No checkmark
                       glyph — just a filled primary-500 dot when on,
                       outlined surface-400 ring when off.  Matches
                       the new icon-sized affordances used elsewhere
                       in the picker. -->
                  <button
                    type="button"
                    role="checkbox"
                    aria-checked={isChecked}
                    aria-label={isChecked ? `Deselect ${entry.name}` : `Select ${entry.name}`}
                    class="w-4 h-4 shrink-0 rounded-full border-2 transition-colors {isChecked
                      ? 'bg-primary-500 border-surface-400 dark:border-surface-500'
                      : 'border-surface-400 dark:border-surface-500 hover:border-surface-600 dark:hover:border-surface-300'}"
                    onclick={(e) => {
                      e.stopPropagation()
                      toggleSelected(entry.path, entry.is_dir)
                    }}
                  ></button>
                {/if}
                {#if iconName}
                  <!-- Folder / plaintext icon at 20 px inside a
                       36 px slot — visibly smaller than the file
                       thumbnail beside it so the user can
                       differentiate at a glance (#160). -->
                  <span class="w-9 h-9 flex items-center justify-center shrink-0 text-surface-500">
                    <Icon name={iconName} size={20} />
                  </span>
                {:else}
                  <!-- File preview — sits inside the same 36 px
                       slot for row alignment, but the visible
                       thumbnail / typed icon caps at 28 px so it
                       matches the rest of the new icon sizes
                       without dominating the row. -->
                  <span class="w-9 h-9 flex items-center justify-center shrink-0">
                    <NcPreview
                      {accountId}
                      path={entry.path}
                      contentType={entry.content_type}
                      filename={entry.name}
                      class="w-7 h-7"
                    />
                  </span>
                {/if}
                <span class="flex-1 truncate text-sm">{entry.name}</span>
                <span class="text-xs text-surface-500">{formatSize(entry.size)}</span>
              </div>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
{/if}

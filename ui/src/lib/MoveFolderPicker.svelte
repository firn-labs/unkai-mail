<script lang="ts">
  /**
   * MoveFolderPicker — modal that lists every folder for an account
   * (#89).  Used by both the "Move" toolbar button in MailView and
   * the "Move to folder…" right-click entry in MailList.  Visual
   * style mirrors the Sidebar's folder list so the user reaches for
   * the same icon they'd reach for in the tree.
   *
   * The picker is intentionally read-only — it doesn't trigger the
   * IMAP MOVE itself.  `onpicked(folderName)` hands the selection
   * back to the caller, which already has the source `{accountId,
   * folder, uid}` context to fire the `move_message` command.
   * Keeping the I/O on the caller side avoids prop drilling and
   * makes this component reusable.
   */

  import * as api from './api'
  import { formatError } from './errors'
  import Icon, { type IconName } from './Icon.svelte'

  interface Folder {
    name: string
    delimiter: string | null
    attributes: string[]
    unread_count: number | null
  }

  interface FolderIconRule {
    keyword: string
    icon: string
  }

  interface Account {
    id: string
    folder_icons?: FolderIconRule[]
    folder_icon_overrides?: Record<string, string>
  }

  let {
    accountId,
    currentFolder,
    accounts = [],
    onpicked,
    onclose,
  }: {
    accountId: string
    /** The mail's current folder.  Disabled in the list (move-to-self
     *  is a noop) and labelled so the user knows why. */
    currentFolder: string
    /** Optional accounts list — the picker uses it to resolve
     *  per-folder icon overrides + keyword rules.  Same shape as
     *  Sidebar's `accounts` prop.  Falls back to a generic 📁 when
     *  nothing matches. */
    accounts?: Account[]
    onpicked: (folderName: string) => void
    onclose: () => void
  } = $props()

  let folders = $state<Folder[]>([])
  let loading = $state(true)
  let error = $state('')

  $effect(() => {
    void (async () => {
      loading = true
      error = ''
      try {
        folders = await api.mail.getCachedFolders({ accountId })
      } catch (e) {
        error = formatError(e) || 'Failed to load folders'
      } finally {
        loading = false
      }
    })()
  })

  // Same folder-icon resolution chain Sidebar uses.  Lifted here so
  // the picker stays self-contained — pulling it into a shared util
  // is a follow-up if a third caller appears.
  /** Discriminated union mirroring `Sidebar.folderIcon` so the
   *  picker and the sidebar render folder rows with identical
   *  iconography (#179).  Standard folders → `Icon`; user-picked
   *  override / keyword rule → emoji string. */
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
    if (has('sent')) return { kind: 'icon', name: 'sent' }
    if (has('draft')) return { kind: 'icon', name: 'drafts' }
    if (has('trash') || has('deleted') || name === 'trash' || name === 'papierkorb')
      return { kind: 'icon', name: 'trash' }
    if (has('junk') || has('spam') || name === 'spam' || name === 'junk')
      return { kind: 'icon', name: 'spam' }
    if (has('flagged') || has('starred')) return { kind: 'icon', name: 'star' }
    if (has('archive')) return { kind: 'icon', name: 'archive' }
    if (name === 'notes' || name.endsWith('/notes')) return { kind: 'icon', name: 'notes' }

    const rules = account?.folder_icons ?? []
    for (const rule of rules) {
      const kw = rule.keyword.trim().toLowerCase()
      if (kw && name.includes(kw)) return { kind: 'emoji', value: rule.icon }
    }
    return { kind: 'icon', name: 'files' }
  }

  function displayName(f: Folder): string {
    if (f.name.toUpperCase() === 'INBOX') return 'Inbox'
    const delim = f.delimiter ?? '/'
    const parts = f.name.split(delim)
    return parts[parts.length - 1] || f.name
  }

  /** Indent depth — counts the path delimiters so subfolders nest
   *  visually under their parent.  E.g. `INBOX/Work/Reports` lands
   *  at depth 2.  Cheaper than building a real tree and good enough
   *  to read the hierarchy at a glance. */
  function depth(f: Folder): number {
    const delim = f.delimiter ?? '/'
    return Math.max(0, f.name.split(delim).length - 1)
  }

  // Same ordering Sidebar uses: standard folders first in canonical
  // order (Inbox → Drafts → Sent → Flagged → Archive → Junk →
  // Trash), then user folders alphabetically.
  function standardRank(f: Folder): number {
    const name = f.name.toLowerCase()
    const attrs = f.attributes.map((a) => a.toLowerCase())
    const has = (k: string) => attrs.some((a) => a.includes(k))
    if (name === 'inbox' || has('inbox')) return 0
    if (has('draft')) return 1
    if (has('sent')) return 2
    if (has('flagged') || has('starred')) return 3
    if (has('archive')) return 4
    if (has('junk') || has('spam') || name === 'spam' || name === 'junk') return 5
    if (has('trash') || has('deleted') || name === 'trash' || name === 'papierkorb') return 6
    return -1
  }

  let sortedFolders = $derived.by(() => {
    const standard = folders
      .filter((f) => standardRank(f) >= 0)
      .sort((a, b) => standardRank(a) - standardRank(b))
    const user = folders
      .filter((f) => standardRank(f) < 0)
      .sort((a, b) => a.name.localeCompare(b.name))
    return [...standard, ...user]
  })

  let filterText = $state('')
  let visibleFolders = $derived.by(() => {
    const q = filterText.trim().toLowerCase()
    if (!q) return sortedFolders
    return sortedFolders.filter((f) => f.name.toLowerCase().includes(q))
  })

  function onPickFolder(name: string) {
    if (name === currentFolder) return
    onpicked(name)
    onclose()
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
  role="dialog"
  aria-modal="true"
  aria-label="Move message to folder"
  tabindex="-1"
  onclick={(e) => {
    if (e.target === e.currentTarget) onclose()
  }}
>
  <div class="glass-float rounded-2xl flex flex-col w-[420px] max-w-[90vw] max-h-[80vh]">
    <header class="px-5 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center justify-between">
      <h2 class="text-base font-semibold">Move to folder</h2>
      <button
        class="text-surface-500 hover:text-surface-900 dark:hover:text-surface-100"
        onclick={onclose}
        aria-label="Close"
      ><Icon name="close" size={18} /></button>
    </header>

    <div class="px-3 py-2 border-b border-surface-200 dark:border-surface-700">
      <input
        type="text"
        class="input w-full text-sm px-2 py-1 rounded-lg"
        placeholder="Filter folders…"
        bind:value={filterText}
      />
    </div>

    <div class="flex-1 overflow-y-auto px-2 py-2">
      {#if loading}
        <p class="px-3 py-2 text-xs text-surface-500">Loading folders…</p>
      {:else if error}
        <p class="px-3 py-2 text-sm text-error-500">{error}</p>
      {:else if visibleFolders.length === 0}
        <p class="px-3 py-2 text-xs text-surface-500">
          {filterText ? 'No folders match.' : 'No folders.'}
        </p>
      {:else}
        {#each visibleFolders as f (f.name)}
          {@const isCurrent = f.name === currentFolder}
          {@const glyph = folderIcon(f)}
          <button
            class="w-full flex items-center gap-2 px-3 py-2 rounded-lg text-sm text-left transition-colors disabled:text-surface-400 disabled:cursor-not-allowed
              {isCurrent
                ? 'bg-surface-200/50 dark:bg-surface-700/50'
                : 'hover:bg-primary-500/10'}"
            style:padding-left={`${0.75 + depth(f) * 1.25}rem`}
            disabled={isCurrent}
            onclick={() => onPickFolder(f.name)}
            title={isCurrent ? 'Already in this folder' : `Move to ${f.name}`}
          >
            {#if glyph.kind === 'icon'}
              <Icon name={glyph.name} size={16} />
            {:else}
              <span>{glyph.value}</span>
            {/if}
            <span class="flex-1 truncate">{displayName(f)}</span>
            {#if isCurrent}
              <span class="text-[10px] text-surface-500 uppercase tracking-wider">Current</span>
            {/if}
          </button>
        {/each}
      {/if}
    </div>
  </div>
</div>

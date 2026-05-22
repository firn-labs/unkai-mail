<script lang="ts">
  /**
   * FilesView — sidebar-routed full-pane Nextcloud file browser.
   *
   * The complement to `NextcloudFilePicker` (which only opens from
   * inside Compose). This view is what the user lands on when they
   * click "Nextcloud Files" in the sidebar's Integrations list.
   *
   * # Per-issue actions
   *
   * The two "browser-context" actions issue #53 calls for:
   *   - **Send as link in mail** — works for files *and* folders. We
   *     call `create_nextcloud_share` for each selection and then
   *     open Compose with the resulting URLs pre-rendered into the
   *     body (Compose's existing share-link block).
   *   - **Send as attachment in mail** — files only (Nextcloud has
   *     no zip-folder endpoint). We download each file's bytes and
   *     hand them to Compose as pre-filled `attachments`.
   *
   * Both actions accept a multi-select (the footer counts feed the
   * button labels) so the user can build "one mail, three files"
   * without juggling separate Compose windows.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { formatError } from './errors'
  import Icon from './Icon.svelte'
  import NextcloudFileBrowser, {
    type FileEntry,
    type NextcloudAccount,
  } from './NextcloudFileBrowser.svelte'
  import NextcloudShareDialog, {
    type ShareLink,
  } from './NextcloudShareDialog.svelte'
  import type { ComposeInitial } from './Compose.svelte'

  interface Attachment {
    filename: string
    content_type: string
    data: number[]
  }

  interface Props {
    onclose: () => void
    /** Open Compose with the given prefill (attachments / share links).
        Wired by App.svelte to the same handler that drives every other
        Compose entry point. */
    oncompose: (initial: ComposeInitial) => void
  }
  let { onclose, oncompose }: Props = $props()

  // Bound from the inner browser. The view's own footer reads these
  // to label/disable buttons and to drive the action commands.
  let accountId = $state('')
  let currentPath = $state('/')
  let selected = $state<Set<string>>(new Set())
  let entries = $state<FileEntry[]>([])
  let accounts = $state<NextcloudAccount[]>([])
  let error = $state('')

  let attaching = $state(false)
  // Snapshot of the selection at the moment "New mail with link" was
  // clicked.  The shared `NextcloudShareDialog` owns the form state
  // (password / permissions / expiry); we just hand off paths +
  // hasFolders and wait for `onresolve` / `oncancel`.
  let shareTarget = $state<{
    paths: string[]
    hasFolders: boolean
  } | null>(null)
  // True while the share dialog is mounted.  The dialog blocks
  // clicks behind it via its backdrop, so this just gates the
  // footer buttons + the Esc handler.
  let sharing = $derived(shareTarget !== null)

  let selectedFileCount = $derived.by(() => {
    let n = 0
    for (const e of entries) if (!e.is_dir && selected.has(e.path)) n++
    return n
  })
  let selectedFolderCount = $derived(selected.size - selectedFileCount)

  function basename(path: string): string {
    return path.split('/').filter(Boolean).pop() ?? path
  }

  /** Per-file download status surfaced as a progress strip while
   *  `sendAsAttachment` runs (#160).  Same shape as the one in
   *  NextcloudFilePicker — keys are NC paths, values cycle
   *  pending → downloading → done | failed. */
  type DownloadStatus =
    | { kind: 'pending' }
    | { kind: 'downloading' }
    | { kind: 'done' }
    | { kind: 'failed'; message: string }
  let downloadStatus = $state<Map<string, DownloadStatus>>(new Map())
  function setDownloadStatus(path: string, status: DownloadStatus) {
    const next = new Map(downloadStatus)
    next.set(path, status)
    downloadStatus = next
  }

  /** Download every selected file (folders are skipped — Nextcloud has
      no zip-folder endpoint) and open Compose with them pre-attached. */
  async function sendAsAttachment() {
    const filePaths = entries
      .filter((e) => !e.is_dir && selected.has(e.path))
      .map((e) => e.path)
    if (filePaths.length === 0) return
    attaching = true
    error = ''
    const seeded = new Map<string, DownloadStatus>()
    for (const p of filePaths) seeded.set(p, { kind: 'pending' })
    downloadStatus = seeded
    try {
      // Same parallelisation rationale as the picker: each invoke is
      // an independent task, so this scales with file count rather
      // than serialising.
      const attachments = await Promise.all(
        filePaths.map(async (p) => {
          setDownloadStatus(p, { kind: 'downloading' })
          try {
            const bytes = await invoke<number[]>('download_nextcloud_file', {
              ncId: accountId,
              path: p,
            })
            const ct =
              entries.find((e) => e.path === p)?.content_type ??
              'application/octet-stream'
            setDownloadStatus(p, { kind: 'done' })
            return {
              filename: basename(p),
              content_type: ct,
              data: bytes,
            } satisfies Attachment
          } catch (e) {
            setDownloadStatus(p, { kind: 'failed', message: formatError(e) || 'Failed' })
            throw e
          }
        }),
      )
      oncompose({ attachments })
    } catch (e) {
      error = formatError(e) || 'Failed to download file(s)'
    } finally {
      attaching = false
    }
  }

  /** Open the share dialog instead of jumping straight to OCS.
      The dialog lets the user opt into password / permissions /
      expiry before any link is minted. */
  function sendAsLink() {
    if (selected.size === 0) return
    const hasFolders =
      entries.some((e) => e.is_dir && selected.has(e.path)) ||
      selectedFolderCount > 0
    shareTarget = {
      paths: Array.from(selected),
      hasFolders,
    }
    error = ''
  }

  /** Dialog resolved with freshly-minted share links.  Project the
      result down to the `{filename, url}` shape `ComposeInitial`
      expects and open Compose pre-filled with the link block. */
  function onShareResolved(links: ShareLink[]) {
    shareTarget = null
    oncompose({
      nextcloudLinks: links.map((l) => ({ filename: l.filename, url: l.url })),
    })
  }

  /**
   * Esc handler for the share dialog (#192).  Wired via
   * `<svelte:window onkeydown>` in the template so the key works
   * wherever focus is in the dialog — the dialog's own input-level
   * handler only catches the password field.
   */
  function onSharePromptKeydown(e: KeyboardEvent) {
    if (e.key !== 'Escape') return
    if (!shareTarget) return
    e.preventDefault()
    shareTarget = null
  }
</script>

<svelte:window onkeydown={onSharePromptKeydown} />

<div class="h-full flex flex-col bg-surface-50 dark:bg-surface-900">
  <!-- Header — same shape as the Calendar / Contacts views so the
       sidebar-routed integrations all feel like one app, not three. -->
  <div
    class="flex items-center justify-between px-6 py-3 border-b border-surface-200 dark:border-surface-700 bg-surface-100 dark:bg-surface-800"
  >
    <div class="flex items-center gap-3">
      <h2 class="text-xl font-semibold">Nextcloud Files</h2>
      {#if currentPath !== '/'}
        <span class="text-sm text-surface-500 font-mono">{currentPath}</span>
      {/if}
    </div>
    <div class="flex items-center gap-2">
      <button class="btn preset-tonal-surface text-sm" onclick={onclose}>
        Close
      </button>
    </div>
  </div>

  <!-- The shared browser fills the rest. The browser itself owns
       account picking, breadcrumbs, listing, and "+ New folder" — we
       only consume its bound state for the action footer. -->
  <div class="flex-1 min-h-0 flex flex-col">
    <NextcloudFileBrowser
      bind:accountId
      bind:currentPath
      bind:selected
      bind:entries
      bind:accounts
      bind:error
    />

    {#if error}
      <p class="px-5 py-2 text-sm text-red-500 border-t border-surface-200 dark:border-surface-700">
        {error}
      </p>
    {/if}

    {#if downloadStatus.size > 0 && (attaching || [...downloadStatus.values()].some((s) => s.kind === 'failed'))}
      <!-- Per-file download status strip (#160).  Mirrors the
           NextcloudFilePicker progress UI so the user sees exactly
           which file is being fetched and which (if any) errored. -->
      <!-- Strip ordering (#160): completed rows pile up at the top,
           the active download sits at the bottom, pending rows
           below it.  No scrollbar — the strip grows to fit. -->
      <div class="px-5 py-2 border-t border-surface-200 dark:border-surface-700 space-y-1.5">
        {#each [...downloadStatus].sort((a, b) => {
          const rank = (k: DownloadStatus['kind']) =>
            k === 'done' || k === 'failed' ? 0 : k === 'downloading' ? 1 : 2
          return rank(a[1].kind) - rank(b[1].kind)
        }) as [path, status] (path)}
          <div class="text-xs">
            <div class="flex items-center gap-2">
              <span class="shrink-0 w-4 h-4 flex items-center justify-center">
                {#if status.kind === 'pending'}
                  <span class="w-2 h-2 rounded-full bg-surface-400"></span>
                {:else if status.kind === 'downloading'}
                  <span class="text-primary-500"><Icon name="loading" size={14} /></span>
                {:else if status.kind === 'done'}
                  <span class="text-success-500"><Icon name="success" size={14} /></span>
                {:else}
                  <span class="text-error-500"><Icon name="error" size={14} /></span>
                {/if}
              </span>
              <span class="flex-1 truncate text-surface-700 dark:text-surface-300">{basename(path)}</span>
              {#if status.kind === 'failed'}
                <span class="shrink-0 text-error-500 truncate max-w-[180px]" title={status.message}>{status.message}</span>
              {:else if status.kind === 'done'}
                <span class="shrink-0 text-success-500">Done</span>
              {:else if status.kind === 'downloading'}
                <span class="shrink-0 text-primary-500">Downloading…</span>
              {:else}
                <span class="shrink-0 text-surface-500">Queued</span>
              {/if}
            </div>
            <!-- Indeterminate per-file bar (#160).  Same component
                 styling as `NextcloudFilePicker` — see the comment
                 there for why we don't drive a real percentage. -->
            <div class="mt-1 ml-6 h-1 rounded-full overflow-hidden bg-surface-200 dark:bg-surface-700 relative">
              {#if status.kind === 'downloading'}
                <span class="nc-indeterminate absolute inset-y-0 left-0 w-1/3 bg-primary-500 rounded-full"></span>
              {:else if status.kind === 'done'}
                <span class="absolute inset-0 bg-success-500"></span>
              {:else if status.kind === 'failed'}
                <span class="absolute inset-0 bg-error-500"></span>
              {/if}
            </div>
          </div>
        {/each}
      </div>
    {/if}

    <!-- Action footer. Both buttons are bulk-aware: select N items and
         the resulting Compose window carries all N attachments / links
         in a single new message. -->
    {#if accounts.length > 0 && accountId}
      <footer class="px-5 py-3 border-t border-surface-200 dark:border-surface-700 flex items-center gap-2">
        <span class="text-xs text-surface-500">
          {#if selected.size === 0}
            Tick a file or folder to share via mail
          {:else if selectedFolderCount === 0}
            {selectedFileCount} file{selectedFileCount === 1 ? '' : 's'} selected
          {:else if selectedFileCount === 0}
            {selectedFolderCount} folder{selectedFolderCount === 1 ? '' : 's'} selected
          {:else}
            {selectedFileCount} file{selectedFileCount === 1 ? '' : 's'},
            {selectedFolderCount} folder{selectedFolderCount === 1 ? '' : 's'} selected
          {/if}
        </span>
        <div class="flex-1"></div>
        <button
          class="btn preset-outlined-primary-500"
          disabled={selected.size === 0 || sharing || attaching}
          onclick={sendAsLink}
          title="Open a new mail with public download links inserted into the body"
        >
          {#if sharing}
            Sharing…
          {:else}
            <Icon name="share-links" size={14} class="inline-block align-text-bottom mr-1.5" />New mail with link
          {/if}
        </button>
        <button
          class="btn preset-filled-primary-500"
          disabled={selectedFileCount === 0 || attaching || sharing}
          onclick={sendAsAttachment}
          title={selectedFileCount === 0 && selectedFolderCount > 0
            ? 'Folders can be shared as a link, but not attached as bytes'
            : 'Open a new mail with the selected files attached'}
        >
          {#if attaching}
            Downloading…
          {:else}
            <Icon name="attachment" size={14} class="inline-block align-text-bottom mr-1.5" />New mail with attachment
          {/if}
        </button>
      </footer>
    {/if}
  </div>
</div>

<!-- Share dialog (password / permissions / expiry).  Shared with
     the Compose-side picker — see NextcloudShareDialog.svelte for
     the modal body itself.  z-50 because no other modal sits
     behind this surface. -->
<NextcloudShareDialog
  open={shareTarget !== null}
  accountId={accountId}
  paths={shareTarget?.paths ?? []}
  hasFolders={shareTarget?.hasFolders ?? false}
  zIndex={50}
  onresolve={onShareResolved}
  oncancel={() => (shareTarget = null)}
/>

<style>
  /* Indeterminate per-file progress head (#160).  Animates
     `transform` rather than `left` so the bar keeps ticking
     while the main thread is busy structured-cloning each
     file's bytes when its IPC resolves — `left` is layout-
     driven and stalls the moment the main thread blocks,
     `transform` runs on the GPU compositor. */
  @keyframes nc-indeterminate {
    0% { transform: translateX(-100%); }
    100% { transform: translateX(400%); }
  }
  .nc-indeterminate {
    animation: nc-indeterminate 1.2s linear infinite;
    will-change: transform;
  }
</style>

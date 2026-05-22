<script lang="ts">
  /**
   * NextcloudFilePicker — modal wrapper around `NextcloudFileBrowser`.
   *
   * Two callers today, both via Compose:
   *   - **Attach mode** (default): the user picks files; we download
   *     each one and hand the bytes back via `onpicked`.
   *   - **Share-as-link mode** (when `onlinks` is set): the user picks
   *     files or folders; we ask the server to mint public share URLs
   *     and return them via `onlinks`.
   *   - **Folder-pick mode** (when `onpickfolder` is set): the user
   *     navigates the tree and picks the *current* folder as a target
   *     (used by "Save attachment to Nextcloud").
   *
   * The browse UI itself lives in `NextcloudFileBrowser` so the
   * sidebar-routed `FilesView` can reuse it without dragging in modal
   * chrome or attach-specific actions.
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

  interface Attachment {
    filename: string
    content_type: string
    data: number[]
  }

  interface Props {
    /** Called when the user attaches the selected files as bytes. */
    onpicked: (attachments: Attachment[]) => void
    /**
     * Called when the user shares the selected files as public links.
     * Optional — callers that don't want the share action just leave
     * it undefined and the button won't render.
     */
    onlinks?: (links: ShareLink[]) => void
    /**
     * If set, the picker switches to **folder-pick mode**: the user
     * navigates the tree and picks the *current folder* as a
     * destination (the per-file checkboxes and Attach/Share buttons
     * are hidden, and a "Save here" button appears in the footer).
     */
    onpickfolder?: (accountId: string, folderPath: string) => void
    /**
     * Optional human-readable label that gets attached to every
     * share created from this picker (#91).  Compose passes the
     * mail's recipient string so each share lands in Nextcloud's
     * "Shared with others" list under "who got this link" rather
     * than the default auto-generated name.  Empty / undefined
     * leaves Nextcloud's auto-naming intact.
     */
    shareLabel?: string
    onclose: () => void
  }
  let {
    onpicked,
    onlinks,
    onpickfolder,
    shareLabel,
    onclose,
  }: Props = $props()

  let pickFolderMode = $derived(onpickfolder != null)

  /**
   * Esc handler for the picker (#192).  Wired via
   * `<svelte:window onkeydown>` in the template.  Two-stage:
   * if the share-prompt sub-modal is open, Esc closes that
   * first (matching the existing input-level handler).
   * Otherwise Esc closes the whole picker via `onclose()`.
   * Inert while `sharing` is in flight so the user can't bail
   * mid-OCS-call and end up with a half-created share.
   */
  function onPickerKeydown(e: KeyboardEvent) {
    if (e.key !== 'Escape') return
    if (sharing) return
    if (shareTarget) {
      e.preventDefault()
      shareTarget = null
      return
    }
    e.preventDefault()
    onclose()
  }

  // Bound from the inner browser — we read these to drive the footer
  // buttons and the download/share actions.
  let accountId = $state('')
  let currentPath = $state('/')
  let selected = $state<Set<string>>(new Set())
  /** Subset of `selected` whose paths are folders.  Tracked
   *  alongside `selected` so file vs. folder counts stay
   *  accurate even after the user navigates to a different
   *  folder mid-selection. */
  let selectedDirs = $state<Set<string>>(new Set())
  let entries = $state<FileEntry[]>([])
  let accounts = $state<NextcloudAccount[]>([])
  let error = $state('')

  let downloading = $state(false)
  // Snapshot of the selection at the moment "Share as link" was
  // clicked — toggling the file tree behind the dialog can't change
  // what gets shared.  The dialog component owns its own form state
  // (password / permissions / expiry); we hand off paths +
  // hasFolders and wait for `onresolve` / `oncancel`.
  let shareTarget = $state<{
    paths: string[]
    hasFolders: boolean
  } | null>(null)
  // True while the share dialog is mounted — used by the picker's
  // own buttons + Esc handler to stay inert while the user is
  // mid-share-flow.  The actual in-flight IPC state lives inside
  // the dialog; from the picker's perspective "dialog open" is
  // close enough — the dialog backdrop blocks any clicks anyway.
  let sharing = $derived(shareTarget !== null)

  /** Per-file download status surfaced as a progress strip while
   *  `attachSelected` runs (#160).  Keys are the full NC paths
   *  the user ticked.  `pending` rows show a queued chip,
   *  `downloading` an active spinner, `done` a green check, and
   *  `failed` a red mark with the underlying error string. */
  type DownloadStatus =
    | { kind: 'pending' }
    | { kind: 'downloading' }
    | { kind: 'done' }
    | { kind: 'failed'; message: string }
  let downloadStatus = $state<Map<string, DownloadStatus>>(new Map())
  function setStatus(path: string, status: DownloadStatus) {
    const next = new Map(downloadStatus)
    next.set(path, status)
    downloadStatus = next
  }

  // Selection split by entry type. Folders can be shared as public
  // links but not attached as bytes (Nextcloud has no zip-folder
  // endpoint, so there's nothing meaningful to download). The footer
  // uses these counts to label and disable buttons appropriately.
  // Selection split is computed straight off the two
  // bindable sets, so it stays correct across folder
  // navigation: selectedDirs always contains *every* folder
  // ever ticked, not just ones currently rendered.
  let selectedFolderCount = $derived(selectedDirs.size)
  let selectedFileCount = $derived(selected.size - selectedDirs.size)

  function basename(path: string): string {
    return path.split('/').filter(Boolean).pop() ?? path
  }

  async function attachSelected() {
    // Pull from `selected` / `selectedDirs` directly so files
    // ticked in folders the user has since navigated away from
    // still get downloaded.  Filtering by the current `entries`
    // list (the previous behaviour) silently dropped them.
    const filePaths = [...selected].filter((p) => !selectedDirs.has(p))
    if (filePaths.length === 0) return
    downloading = true
    error = ''
    // Seed every selected path as pending so the user sees the
    // full list of files the picker is about to fetch.  Each row
    // flips to `downloading` immediately before its IPC fires
    // and to `done` / `failed` when the response lands (#160).
    const seeded = new Map<string, DownloadStatus>()
    for (const p of filePaths) seeded.set(p, { kind: 'pending' })
    downloadStatus = seeded
    try {
      // Run all downloads in parallel — Tauri bridges each invoke to
      // its own async task so this genuinely parallelises.
      const results = await Promise.all(
        filePaths.map(async (p) => {
          setStatus(p, { kind: 'downloading' })
          try {
            const bytes = await invoke<number[]>('download_nextcloud_file', {
              ncId: accountId,
              path: p,
            })
            // Content-type from the current folder's entries when
            // available; fall back to a neutral default for files
            // selected in other folders (the SMTP build path
            // re-derives from filename when this is unset).
            const ct =
              entries.find((e) => e.path === p)?.content_type ??
              'application/octet-stream'
            setStatus(p, { kind: 'done' })
            return {
              filename: basename(p),
              content_type: ct,
              data: bytes,
            } satisfies Attachment
          } catch (e) {
            setStatus(p, { kind: 'failed', message: formatError(e) || 'Failed' })
            throw e
          }
        }),
      )
      onpicked(results)
      onclose()
    } catch (e) {
      error = formatError(e) || 'Failed to download file(s)'
    } finally {
      downloading = false
    }
  }

  /** Open the share dialog instead of jumping straight to OCS.
      The dialog lets the user opt into a password, permissions
      bitmask, and expiry before any link is minted — no way to
      forget those gates, no need to delete + recreate a share
      if the user changes their mind mid-click. */
  function shareSelected() {
    if (selected.size === 0 || !onlinks) return
    // `selectedDirs` is the source of truth for "is any of the
    // ticked paths a folder" — survives folder navigation.
    shareTarget = {
      paths: Array.from(selected),
      hasFolders: selectedDirs.size > 0,
    }
    error = ''
  }

  /** Dialog resolved with freshly-minted share links.  Hand them
      off to the Compose caller and close the picker. */
  function onShareResolved(links: ShareLink[]) {
    shareTarget = null
    onlinks?.(links)
    onclose()
  }
</script>

<svelte:window onkeydown={onPickerKeydown} />

<div
  class="fixed inset-0 z-60 flex items-center justify-center bg-black/50"
  role="dialog"
  aria-modal="true"
>
  <div class="w-160 max-h-[80vh] bg-surface-50 dark:bg-surface-900 rounded-lg shadow-xl flex flex-col">
    <header class="px-5 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center justify-between">
      <h2 class="text-base font-semibold">
        {pickFolderMode ? 'Save to Nextcloud' : 'Attach from Nextcloud'}
      </h2>
      <button
        class="text-surface-500 hover:text-surface-900 dark:hover:text-surface-100"
        onclick={onclose}
        aria-label="Close"
      >✕</button>
    </header>

    <NextcloudFileBrowser
      {pickFolderMode}
      bind:accountId
      bind:currentPath
      bind:selected
      bind:selectedDirs
      bind:entries
      bind:accounts
      bind:error
    />

    {#if error}
      <p class="px-5 py-2 text-sm text-red-500 border-t border-surface-200 dark:border-surface-700">
        {error}
      </p>
    {/if}

    {#if downloadStatus.size > 0 && (downloading || [...downloadStatus.values()].some((s) => s.kind === 'failed'))}
      <!-- Per-file download status strip (#160).  Rendered while
           the picker is fetching, and stays visible if any file
           failed so the user can read the error before
           dismissing.  Successful runs auto-dismiss when the
           picker closes via `onpicked`. -->
      <!-- Strip ordering (#160): completed rows pile up at the top,
           the active download sits at the bottom of the visible
           list, with pending rows below it.  No scrollbar — the
           strip just grows to fit, so the user always sees every
           in-flight + completed file without scrubbing. -->
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
            <!-- Per-file progress bar (#160).  IPC `download_nextcloud_file`
                 returns the entire payload at once with no chunked progress
                 events, so the bar is indeterminate — animated head sliding
                 left → right communicates "this row is actively working"
                 without claiming a percentage we can't measure.  Pending
                 rows show a quiet grey track; done / failed rows fill the
                 track in their semantic colour so the user can scan a long
                 list and spot the one that errored. -->
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

    <footer class="px-5 py-3 border-t border-surface-200 dark:border-surface-700 flex items-center gap-2">
      <span class="text-xs text-surface-500">
        {#if pickFolderMode}
          Saving to <span class="font-mono">{currentPath}</span>
        {:else if selected.size === 0}
          Nothing selected
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
      <button class="btn preset-outlined-surface-500" onclick={onclose}>Cancel</button>
      {#if pickFolderMode}
        <button
          class="btn preset-filled-primary-500"
          disabled={!accountId}
          onclick={() => {
            onpickfolder?.(accountId, currentPath)
            onclose()
          }}
          title="Save the file into this folder"
        >
          💾 Save here
        </button>
      {:else}
        {#if onlinks}
          <button
            class="btn preset-outlined-primary-500"
            disabled={selected.size === 0 || sharing || downloading}
            onclick={shareSelected}
            title="Insert public download links into the email body"
          >
            {#if sharing}
              Sharing…
            {:else}
              <Icon name="share-links" size={14} class="inline-block align-text-bottom mr-1.5" />Share as link
            {/if}
          </button>
        {/if}
        <button
          class="btn preset-filled-primary-500"
          disabled={selectedFileCount === 0 || downloading || sharing}
          onclick={attachSelected}
          title={selectedFileCount === 0 && selectedFolderCount > 0
            ? 'Folders can be shared as a link, but not attached as bytes'
            : 'Download selected files and attach them to the email'}
        >
          {#if downloading}
            Downloading…
          {:else}
            <Icon name="attachment" size={14} class="inline-block align-text-bottom mr-1.5" />Attach
          {/if}
        </button>
      {/if}
    </footer>
  </div>
</div>

<!-- Share dialog (password / permissions / expiry).  Sits over the
     picker's own modal (z-70 vs z-60) so dismissing it returns
     focus to the picker without unmounting the selection. -->
<NextcloudShareDialog
  open={shareTarget !== null}
  accountId={accountId}
  paths={shareTarget?.paths ?? []}
  hasFolders={shareTarget?.hasFolders ?? false}
  shareLabel={shareLabel}
  zIndex={70}
  onresolve={onShareResolved}
  oncancel={() => (shareTarget = null)}
/>

<style>
  /* Indeterminate per-file progress head (#160).  Slides a
     short fill segment across the track so an active download
     reads as "in flight" even though the IPC has no chunked
     progress events to drive a real percentage. */
  /* Animate `transform`, not `left`.  The IPC's structured-
     clone of multi-MB payload bytes blocks the JS main thread
     for hundreds of ms when each download lands; main-thread
     animations (left / top / width) freeze for that whole
     window, which read as "the bar is stuck at 30%".  Transform
     animations stay on the GPU compositor and keep ticking
     while the main thread chews the bytes. */
  @keyframes nc-indeterminate {
    0% { transform: translateX(-100%); }
    100% { transform: translateX(400%); }
  }
  .nc-indeterminate {
    animation: nc-indeterminate 1.2s linear infinite;
    will-change: transform;
  }
</style>

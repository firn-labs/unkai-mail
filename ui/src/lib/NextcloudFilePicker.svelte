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
  import DateField from './DateField.svelte'
  import { formatError } from './errors'
  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'
  import NextcloudFileBrowser, {
    type FileEntry,
    type NextcloudAccount,
  } from './NextcloudFileBrowser.svelte'

  interface Attachment {
    filename: string
    content_type: string
    data: number[]
  }
  /** A "share as link" result.
   *
   *  - `filename` / `url` — what the body block needs.
   *  - `id` / `ncId`     — what `update_nextcloud_share_label`
   *    needs so Compose can re-PUT the label whenever the
   *    recipient list changes after the share was minted (#91).
   */
  export interface ShareLink {
    filename: string
    url: string
    id: string
    ncId: string
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
    if (sharePrompt) {
      e.preventDefault()
      sharePrompt = null
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
  let sharing = $state(false)

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

  // Password-protect-the-share modal. `null` = the prompt isn't open;
  // `paths` is the snapshot of selection at the moment the user
  // clicked "Share as link" so toggling the file tree behind the
  // modal can't change what gets shared. `password` is the in-flight
  // input — empty means "no password" (omitted from the OCS request,
  // which keeps the share open as before).  `permissions` is the
  // OCS bitfield value chosen via the dropdown — defaults to
  // read-only so existing flows behave the same when the user
  // clicks straight through.
  let sharePrompt = $state<{
    paths: string[]
    password: string
    permissions: number
    /** Whether any of the selected paths is a folder.  Drives which
     *  permission options the dropdown shows — "Upload + edit" and
     *  "File drop" only make sense for folder shares (file shares
     *  can't have new files dropped into them by definition). */
    hasFolders: boolean
    /** Whether an expiration date should be sent to the OCS endpoint
     *  (#324).  Off by default — matches Nextcloud's own share UI
     *  where a separate "Set expiration date" toggle gates the
     *  picker.  Lets the user share without an expiry in a single
     *  click. */
    setExpiration: boolean
    /** ISO `YYYY-MM-DD` from the DateField.  Empty when `setExpiration`
     *  is off; only sent to the backend when the toggle is on AND
     *  the field is non-empty (commitShare guards both). */
    expireDate: string
  } | null>(null)

  /** Default expiration suggestion when the user first ticks the
   *  "Set expiration date" toggle — 7 days out, in the same
   *  `YYYY-MM-DD` shape DateField round-trips through.  Picked over
   *  "today" so the recipient has a usable window without further
   *  clicks; the user can still drill to any date via the calendar
   *  popover. */
  function defaultExpiry(): string {
    const d = new Date()
    d.setDate(d.getDate() + 7)
    const pad = (n: number) => String(n).padStart(2, '0')
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
  }

  /** Common public-link permission combinations Nextcloud's own
   *  share UI exposes.  The bitfield (1 read, 2 update, 4 create,
   *  8 delete, 16 share) gets sent to the OCS endpoint verbatim. */
  /** Permission combinations Nextcloud's own share UI exposes.
   *  `folderOnly` entries are filtered out when the selection is
   *  pure files — "Upload + edit" and "File drop" semantically
   *  only make sense for folder shares; offering them for a file
   *  share would surface a Nextcloud-side rejection ("invalid
   *  permissions"). */
  const PERMISSION_OPTIONS = [
    {
      value: 1,
      label: 'View only',
      hint: 'Recipient can read / download.',
      folderOnly: false,
    },
    {
      value: 3,
      label: 'View and edit',
      hint: 'Recipient can edit the file in Nextcloud.',
      folderOnly: false,
    },
    {
      value: 15,
      label: 'View, edit, upload, delete',
      hint: 'Folder share with full read-write — recipient can drop files in and modify existing ones.',
      folderOnly: true,
    },
    {
      value: 4,
      label: 'File drop (upload only)',
      hint: 'Folder share where recipients can upload but not see the contents.',
      folderOnly: true,
    },
  ] as const

  function visiblePermissionOptions(hasFolders: boolean) {
    return PERMISSION_OPTIONS.filter((o) => !o.folderOnly || hasFolders)
  }

  function permHint(value: number): string {
    return PERMISSION_OPTIONS.find((o) => o.value === value)?.hint ?? ''
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

  /** Open the password prompt instead of jumping straight to OCS.
      The modal lets the user opt into a password (or skip with
      Enter / "Share without password") before any link is minted —
      no way to forget the password gate, no need to delete + recreate
      a share if the user changes their mind mid-click. */
  function shareSelected() {
    if (selected.size === 0 || !onlinks) return
    // `selectedDirs` is the source of truth for "is any of the
    // ticked paths a folder" — survives folder navigation.
    const hasFolders = selectedDirs.size > 0
    sharePrompt = {
      paths: Array.from(selected),
      password: '',
      permissions: 1, // View-only by default — matches Nextcloud's own picker.
      hasFolders,
      setExpiration: false,
      expireDate: '',
    }
    error = ''
  }

  /** Run the actual create_nextcloud_share calls with the password
      the user picked (empty string = no password, omitted from the
      OCS form on the Rust side). Same error-surface as the previous
      direct flow. */
  async function commitShare() {
    if (!sharePrompt || !onlinks) return
    const { paths, password, permissions, setExpiration, expireDate } =
      sharePrompt
    sharing = true
    error = ''
    try {
      const pw = password.trim() ? password : null
      // Only send `expireDate` when the user actually opted into it.
      // An empty `expireDate=` posted to OCS is parsed as a bad date
      // and rejects the whole share, so the toggle-off path must
      // omit the field entirely.
      const exp = setExpiration && expireDate ? expireDate : null
      const results = await Promise.all(
        paths.map(async (p) => {
          const r = await invoke<{ id: string; url: string }>(
            'create_nextcloud_share',
            {
              ncId: accountId,
              path: p,
              password: pw,
              label: shareLabel?.trim() || null,
              permissions,
              expireDate: exp,
            },
          )
          return {
            filename: basename(p),
            url: r.url,
            id: r.id,
            ncId: accountId,
          } satisfies ShareLink
        }),
      )
      sharePrompt = null
      onlinks(results)
      onclose()
    } catch (e) {
      error = formatError(e) || 'Failed to create share link(s)'
    } finally {
      sharing = false
    }
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

<!-- Password prompt for the public share link. Layered on top of
     the picker's own modal (z-70 vs z-60) so dismissing it returns
     focus to the picker without unmounting the selection. The
     "Share without password" path commits with an empty password,
     which the Rust side translates to omitting the OCS `password`
     param entirely — keeps the previous "no-password share" flow
     reachable in one click. -->
{#if sharePrompt}
  <div
    class="fixed inset-0 z-70 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget && !sharing) sharePrompt = null }}
  >
    <div class="bg-surface-50 dark:bg-surface-900 rounded-lg shadow-xl w-[28rem] max-w-full p-5">
      <h3 class="text-base font-semibold mb-1">Password-protect link?</h3>
      <p class="text-xs text-surface-500 mb-3">
        {sharePrompt.paths.length === 1
          ? 'Anyone with the link can open the file.'
          : `Anyone with each link can open ${sharePrompt.paths.length} files.`}
        Setting a password gates the recipient behind it; leave it empty
        to share without one.
      </p>

      <label class="block text-xs text-surface-500 mb-1" for="share-pw">Password (optional)</label>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        id="share-pw"
        type="password"
        class="input w-full text-sm px-2 py-1.5 rounded-md mb-3"
        placeholder="Leave blank for no password"
        bind:value={sharePrompt.password}
        disabled={sharing}
        autofocus
        onkeydown={(e) => {
          if (e.key === 'Enter') { e.preventDefault(); void commitShare() }
          else if (e.key === 'Escape' && !sharing) { e.preventDefault(); sharePrompt = null }
        }}
      />

      <!-- Permissions dropdown — mirrors Nextcloud's own share UI.
           The bitmask values map to OCS's `permissions` form field.
           File-only flows (where the picker is used to attach a
           single document) practically only use 1 / 3; the upload
           variants ride along for folder shares. -->
      <label class="block text-xs text-surface-500 mb-1" for="share-perms">Permissions</label>
      <select
        id="share-perms"
        class="select w-full text-sm px-2 py-1.5 rounded-md mb-1"
        bind:value={sharePrompt.permissions}
        disabled={sharing}
      >
        {#each visiblePermissionOptions(sharePrompt.hasFolders) as opt}
          <option value={opt.value}>{opt.label}</option>
        {/each}
      </select>
      <p class="text-[11px] text-surface-500 mb-3">
        {permHint(sharePrompt.permissions)}
      </p>

      <!-- Expiration date (#324).  Nextcloud's OCS endpoint takes an
           optional `expireDate=YYYY-MM-DD`; after that date the
           public link returns a "Link expired" page.  We gate the
           DateField behind a checkbox so the no-expiry default
           stays one click away, matching Nextcloud's own UI. -->
      <label class="flex items-center gap-2 mb-2 text-xs text-surface-700 dark:text-surface-300 cursor-pointer">
        <input
          type="checkbox"
          class="checkbox"
          bind:checked={sharePrompt.setExpiration}
          disabled={sharing}
          onchange={() => {
            // Seed a sensible default the first time the user opts in,
            // otherwise reopening the toggle would land on an empty
            // field that DateField renders as "Pick a date".
            if (sharePrompt && sharePrompt.setExpiration && !sharePrompt.expireDate) {
              sharePrompt.expireDate = defaultExpiry()
            }
          }}
        />
        <span>{m.nc_share_set_expiration_label()}</span>
      </label>
      {#if sharePrompt.setExpiration}
        <div class="mb-3">
          <DateField
            bind:value={sharePrompt.expireDate}
            ariaLabel={m.nc_share_expiration_aria()}
          />
          <p class="text-[11px] text-surface-500 mt-1">
            {m.nc_share_expiration_hint()}
          </p>
        </div>
      {/if}

      {#if error}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{error}</p>
      {/if}

      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500 shrink-0"
          disabled={sharing}
          onclick={() => (sharePrompt = null)}
        >Cancel</button>
        <button
          class="btn preset-filled-primary-500 shrink-0 whitespace-nowrap"
          disabled={sharing}
          onclick={() => void commitShare()}
        >
          {#if sharing}Sharing…{:else}Share{/if}
        </button>
      </div>
    </div>
  </div>
{/if}

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

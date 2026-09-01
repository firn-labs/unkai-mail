<script lang="ts">
  /**
   * SharesView — sidebar-routed full-pane Nextcloud share manager (#117).
   *
   * Mirrors TalkView's header + scrollable body shape so the rail-
   * routed integration views all feel like one app.  Lists every
   * public link share the user owns across their connected Nextcloud
   * account(s) and lets them:
   *
   *   - copy the public URL to the clipboard
   *   - open the share in their browser
   *   - edit password / permissions / expiry
   *   - revoke the share entirely
   *
   * # Why we don't cache shares
   *
   * The list is small (typically a few dozen rows) and a Nextcloud
   * sharing change made elsewhere — Compose dropping a new link,
   * the web UI revoking one — would otherwise leave us stale.  We
   * refetch on a 60s timer plus on demand via the refresh button.
   * Same rationale as TalkView's room list: pure UI cache, no SQLite
   * layer.
   *
   * # Re-using NextcloudShareDialog
   *
   * That dialog was built for *creating* shares; the management view
   * needs an *edit* affordance that pre-fills the existing values and
   * speaks PUT instead of POST.  Rather than overload the create
   * dialog with a mode switch (which would also have to seed values
   * and surface a "leave password as-is" tri-state), this view ships
   * its own lighter edit modal.  The two dialogs share the same
   * permission-bitmask vocabulary and DateField for expiry so the
   * visual language stays consistent.
   */

  import * as api from './api'
  import { isNextcloudSource } from './ncSources'
  import { onDestroy, onMount } from 'svelte'
  import DateField from './DateField.svelte'
  import { formatError } from './errors'
  import FileTypeIcon from './FileTypeIcon.svelte'
  import Icon from './Icon.svelte'
  import PasswordInput from './PasswordInput.svelte'
  import SearchInput from './SearchInput.svelte'
  import { m } from '../paraglide/messages'

  interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
  }

  /** Row shape mirrors the Rust `NextcloudShareRow` struct. */
  interface ShareRow {
    nc_id: string
    id: string
    path: string
    item_type: string
    url: string
    token: string
    label: string | null
    permissions: number
    has_password: boolean
    expiration: string | null
    stime: number
    mimetype: string
  }

  // No props — navigation back to the inbox is owned by the
  // IconRail (clicking another account avatar or rail entry routes
  // away).  The view used to carry an `onclose` callback for a
  // dedicated Close button in the header but that button has been
  // retired since it duplicated the rail's job.

  let accounts = $state<NextcloudAccount[]>([])
  let accountId = $state('')
  let shares = $state<ShareRow[]>([])
  let loading = $state(false)
  let error = $state('')
  /** Free-text filter for the current list.  Case-insensitive
   *  substring match across the share's filename, full path, and
   *  (when present) user-supplied label.  Kept purely client-side
   *  — the share list is small enough that round-tripping to OCS
   *  per keystroke would be wasteful, and filtering in memory keeps
   *  the input feel instant. */
  let searchQuery = $state('')
  const filteredShares = $derived.by(() => {
    const q = searchQuery.trim().toLowerCase()
    if (!q) return shares
    return shares.filter((s) => {
      if (basename(s.path).toLowerCase().includes(q)) return true
      if (s.path.toLowerCase().includes(q)) return true
      if (s.label && s.label.toLowerCase().includes(q)) return true
      return false
    })
  })

  // Periodic refresh — slower than Talk's 30s because share lists
  // don't carry urgent "someone's waiting on you" state.  60s is
  // enough to catch a share created in the web UI without spamming
  // the OCS endpoint.
  const REFRESH_INTERVAL_MS = 60_000
  let pollTimer: number | null = null

  onMount(async () => {
    await loadAccounts()
  })

  onDestroy(() => {
    if (pollTimer !== null) window.clearInterval(pollTimer)
  })

  async function loadAccounts() {
    try {
      // Nextcloud-app feature — skip generic-DAV / local sources (#413).
      const list = (
        await api.nextcloud.getNextcloudAccounts()
      ).filter(isNextcloudSource)
      accounts = list
      if (list.length === 1 && !accountId) {
        accountId = list[0].id
        await refresh()
        startPolling()
      }
    } catch (e) {
      error = formatError(e) || m.shares_view_load_accounts_error()
    }
  }

  async function selectAccount(id: string) {
    accountId = id
    shares = []
    await refresh()
    startPolling()
  }

  function startPolling() {
    if (pollTimer !== null) window.clearInterval(pollTimer)
    pollTimer = window.setInterval(() => {
      void refresh({ silent: true })
    }, REFRESH_INTERVAL_MS)
  }

  async function refresh(opts: { silent?: boolean } = {}) {
    if (!accountId) return
    if (!opts.silent) loading = true
    if (!opts.silent) error = ''
    try {
      const list = await api.nextcloud.listNextcloudShares({
        ncId: accountId,
      })
      // Newest first — the user's most recent share is the one most
      // likely to be on their mind, mirroring how the web UI orders
      // the "Shared with others" list.
      list.sort((a, b) => b.stime - a.stime)
      shares = list
    } catch (e) {
      if (!opts.silent) error = formatError(e) || m.shares_view_load_shares_error()
    } finally {
      if (!opts.silent) loading = false
    }
  }

  function basename(path: string): string {
    return path.split('/').filter(Boolean).pop() ?? path
  }

  function dirname(path: string): string {
    const parts = path.split('/').filter(Boolean)
    parts.pop()
    return parts.length === 0 ? '/' : '/' + parts.join('/')
  }

  /** Format a `YYYY-MM-DD` calendar date the way the user's locale
   *  writes dates (`18.06.2026` in de, `6/18/2026` in en-US, etc.).
   *
   *  We deliberately parse the parts and build the `Date` via the
   *  local-zone `Date(y, m, d)` ctor instead of `new Date("YYYY-MM-DD")`
   *  — the string-arg form interprets the value as UTC midnight, which
   *  in negative-offset zones rolls back to the previous day and would
   *  silently shift the displayed expiry one day earlier. */
  function formatExpiryDate(iso: string): string {
    const [yStr, mStr, dStr] = iso.split('-')
    const y = Number(yStr)
    const m = Number(mStr)
    const d = Number(dStr)
    if (!y || !m || !d) return iso
    const date = new Date(y, m - 1, d)
    return date.toLocaleDateString()
  }

  function formatRelative(unix: number): string {
    if (!unix) return ''
    const now = Date.now() / 1000
    const delta = now - unix
    if (delta < 60) return m.shares_view_row_just_now()
    if (delta < 3600) return `${Math.floor(delta / 60)}m ago`
    if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`
    if (delta < 7 * 86400) return `${Math.floor(delta / 86400)}d ago`
    return new Date(unix * 1000).toLocaleDateString()
  }

  /** Same permission vocabulary the create dialog surfaces — kept
   *  in sync by hand because pulling NextcloudShareDialog's private
   *  constant into a shared module would force a refactor with no
   *  other caller asking for it. */
  const PERMISSION_OPTIONS = [
    { value: 1, label: () => m.shares_view_perm_view_only() },
    { value: 3, label: () => m.shares_view_perm_view_edit() },
    { value: 15, label: () => m.shares_view_perm_full() },
    { value: 4, label: () => m.shares_view_perm_filedrop() },
  ] as const

  /** Strip Nextcloud's "reshare" bit (16) before matching against the
   *  canonical dropdown values.  Public-link shares come back from
   *  the server with bit 16 set as a bookkeeping artefact even
   *  though there's no real reshare affordance on a public link
   *  itself — Nextcloud's own picker hides this bit too.  Result:
   *  `17` (1 + 16) reads as "View only" instead of "Custom (17)",
   *  `19` (3 + 16) reads as "View and edit", and so on. */
  function snapPermissionBitsForDisplay(value: number): number {
    const stripped = value & ~16
    // If stripping the reshare bit lands us on a canonical option,
    // surface that; otherwise we genuinely have an exotic
    // combination and fall back to the raw value so the chip /
    // dropdown is at least truthful.
    if (PERMISSION_OPTIONS.some((o) => o.value === stripped)) return stripped
    return value
  }

  function permissionLabel(value: number): string {
    const snapped = snapPermissionBitsForDisplay(value)
    return (
      PERMISSION_OPTIONS.find((o) => o.value === snapped)?.label() ??
      m.shares_view_perm_custom({ value: String(snapped) })
    )
  }

  // ── Copy-to-clipboard with a transient "copied" tick ────────
  let copiedId = $state<string | null>(null)
  let copiedTimer: number | null = null
  async function copyUrl(row: ShareRow) {
    try {
      await navigator.clipboard.writeText(row.url)
      copiedId = row.id
      if (copiedTimer) window.clearTimeout(copiedTimer)
      copiedTimer = window.setTimeout(() => {
        copiedId = null
      }, 1600)
    } catch (e) {
      error = formatError(e) || m.shares_view_copy_error()
    }
  }

  function openInBrowser(row: ShareRow) {
    void api.system.openUrl({ url: row.url })
  }

  // ── Delete ──────────────────────────────────────────────────
  let deletingId = $state<string | null>(null)
  async function deleteShare(row: ShareRow) {
    const ok = window.confirm(
      m.shares_view_revoke_confirm({ name: basename(row.path) }),
    )
    if (!ok) return
    deletingId = row.id
    try {
      await api.nextcloud.deleteNextcloudShare({
        ncId: row.nc_id,
        shareId: row.id,
      })
      shares = shares.filter((s) => s.id !== row.id)
    } catch (e) {
      error = formatError(e) || m.shares_view_delete_error()
    } finally {
      deletingId = null
    }
  }

  // ── Edit modal (password / permissions / expiry) ────────────
  // Single-row edit affordance; opens with the row's current values
  // pre-filled so the user can tweak the bits they care about and
  // PUT the lot back in one go.
  let editing = $state<ShareRow | null>(null)
  let editPermissions = $state(1)
  // Three password modes: keep the existing one (default), clear it,
  // or replace it with a new value.  Submitting "replace" with an
  // empty string is silently treated as "clear" by Nextcloud which
  // would surprise the user, so we keep the modes explicit.
  type PasswordMode = 'keep' | 'clear' | 'replace'
  let editPasswordMode = $state<PasswordMode>('keep')
  let editPasswordValue = $state('')
  let editSetExpiration = $state(false)
  let editExpireDate = $state('')
  let editing_busy = $state(false)
  let editError = $state('')

  /** Row.permissions can come back as a bitmask that doesn't match
   *  one of the canonical dropdown options (e.g. the Nextcloud web
   *  UI allows finer-grained custom combinations).  When that
   *  happens, fall through to the closest representative option so
   *  the dropdown has *something* selected and the user sees a
   *  truthful starting state.  Mapping: any value with the "create"
   *  bit but not "update" → file-drop; with both update + create →
   *  full read-write; with update but no create → edit; everything
   *  else → view-only. */
  function snapToCanonicalPermission(p: number): number {
    if (PERMISSION_OPTIONS.some((o) => o.value === p)) return p
    // Strip the reshare bit first — most "non-canonical" values in
    // practice are just (canonical | 16), so dropping it lands us on
    // a clean dropdown option without losing real information.
    const stripped = p & ~16
    if (PERMISSION_OPTIONS.some((o) => o.value === stripped)) return stripped
    const hasUpdate = (stripped & 2) !== 0
    const hasCreate = (stripped & 4) !== 0
    if (hasCreate && !hasUpdate) return 4
    if (hasUpdate && hasCreate) return 15
    if (hasUpdate) return 3
    return 1
  }

  function openEdit(row: ShareRow) {
    editing = row
    editPermissions = snapToCanonicalPermission(row.permissions)
    editPasswordMode = 'keep'
    editPasswordValue = ''
    editSetExpiration = !!row.expiration
    editExpireDate = row.expiration ?? ''
    editError = ''
    editing_busy = false
  }

  function cancelEdit() {
    if (editing_busy) return
    editing = null
  }

  async function commitEdit() {
    if (!editing) return
    editing_busy = true
    editError = ''

    // Map UI state to backend payload.  `null` / undefined skip the
    // field; empty string is the "clear" sentinel the backend
    // forwards verbatim to Nextcloud's OCS PUT.
    let password: string | null = null
    if (editPasswordMode === 'clear') password = ''
    else if (editPasswordMode === 'replace') password = editPasswordValue

    const expireDate: string | null = editSetExpiration
      ? editExpireDate || null
      : ''

    const permissionsChanged = editPermissions !== editing.permissions
    const payload: Parameters<typeof api.nextcloud.updateNextcloudShare>[0] = {
      ncId: editing.nc_id,
      shareId: editing.id,
    }
    if (password !== null) payload.password = password
    if (permissionsChanged) payload.permissions = editPermissions
    // Expiry: send "" when user cleared a previously-set date, send
    // the new date when set, skip the field when neither holds.
    if (editSetExpiration && editExpireDate) {
      payload.expireDate = editExpireDate
    } else if (!editSetExpiration && editing.expiration) {
      payload.expireDate = ''
    }

    try {
      await api.nextcloud.updateNextcloudShare(payload)
      // Optimistically reflect the change in the local row so the UI
      // doesn't lag behind the next refresh tick.
      const idx = shares.findIndex((s) => s.id === editing!.id)
      if (idx >= 0) {
        const next = { ...shares[idx] }
        if (permissionsChanged) next.permissions = editPermissions
        if (password !== null) next.has_password = password.length > 0
        if (payload.expireDate !== undefined) {
          next.expiration = (payload.expireDate as string) || null
        }
        shares = [
          ...shares.slice(0, idx),
          next,
          ...shares.slice(idx + 1),
        ]
      }
      editing = null
    } catch (e) {
      editError = formatError(e) || m.shares_view_update_error()
    } finally {
      editing_busy = false
    }
  }

  /** Esc / outside-click closes the edit modal — mirrors the share
   *  *create* dialog's UX so muscle memory carries between the two
   *  surfaces. */
  function onEditKeydown(e: KeyboardEvent) {
    if (!editing) return
    if (e.key === 'Escape' && !editing_busy) {
      e.preventDefault()
      cancelEdit()
    }
  }
</script>

<svelte:window onkeydown={onEditKeydown} />

<div class="h-full flex flex-col bg-surface-50 dark:bg-surface-900">
  <!-- Stacked header (#522): title above its icon-only actions,
       docked LEFT so the controls stay in the viewing angle on
       wide monitors; search centered.  Each side slot is `flex-1`
       (the right one an empty spacer) so the centered slot remains
       visually centered on the window regardless of how wide the
       left cluster ends up — without the symmetric flex-1 the
       search drifts off-center as the title/actions change. -->
  <div
    class="flex items-center gap-3 px-6 py-3 border-b glass-panel"
  >
    <div class="flex-1 min-w-0 flex flex-col items-start gap-2">
      <h2 class="text-xl font-semibold truncate">{m.shares_view_title()}</h2>
      <div class="flex items-center gap-2 shrink-0">
        <button
          class="btn btn-sm preset-tonal-surface inline-flex items-center justify-center"
          disabled={!accountId || loading}
          onclick={() => refresh()}
          title={loading ? m.shares_view_refreshing() : m.shares_view_refresh_title()}
          aria-label={loading ? m.shares_view_refreshing() : m.shares_view_refresh()}
        ><Icon name={loading ? 'loading' : 'refresh'} size={14} /></button>
      </div>
    </div>
    <div class="flex-1 flex justify-center min-w-0">
      <SearchInput
        bind:value={searchQuery}
        placeholder={m.shares_view_search_placeholder()}
        class="w-full max-w-md"
      />
    </div>
    <div class="flex-1"></div>
  </div>

  {#if accounts.length === 0}
    <div class="p-6 text-sm text-surface-500">
      {@html m.shares_view_no_account_html()}
    </div>
  {:else}
    {#if accounts.length > 1}
      <div class="px-5 py-2 border-b border-surface-200 dark:border-surface-700 flex items-center gap-2">
        <label for="shares-account" class="text-xs text-surface-500">{m.shares_view_account_label()}</label>
        <select
          id="shares-account"
          class="select text-sm"
          value={accountId}
          onchange={(e) => selectAccount((e.target as HTMLSelectElement).value)}
        >
          <option value="" disabled>{m.shares_view_account_placeholder()}</option>
          {#each accounts as acc (acc.id)}
            <option value={acc.id}>{acc.display_name ?? acc.username} ({acc.server_url})</option>
          {/each}
        </select>
      </div>
    {/if}

    {#if error}
      <p class="px-5 py-2 text-sm text-error-500">{error}</p>
    {/if}

    {#if !accountId}
      <p class="p-6 text-sm text-surface-500">{m.shares_view_pick_account_hint()}</p>
    {:else if loading && shares.length === 0}
      <p class="p-6 text-sm text-surface-500">{m.shares_view_loading()}</p>
    {:else if shares.length === 0}
      <div class="p-6 text-sm text-surface-500">
        {@html m.shares_view_empty_html()}
      </div>
    {:else if filteredShares.length === 0}
      <!-- Non-empty list but filtered down to nothing — separate
           empty state so the user understands the search has hit
           zero rather than the account having no shares at all. -->
      <p class="p-6 text-sm text-surface-500">{m.shares_view_no_matches()}</p>
    {:else}
      <div class="flex-1 overflow-y-auto">
        <ul class="divide-y divide-surface-200 dark:divide-surface-800">
          {#each filteredShares as row (row.id)}
            {@const folder = dirname(row.path)}
            {@const isFolder = row.item_type === 'folder'}
            <li class="px-5 py-3 flex items-center gap-3 hover:bg-primary-500/10">
              <span class="flex-shrink-0 text-surface-600 dark:text-surface-300">
                {#if isFolder}
                  <Icon name="files" size={20} />
                {:else}
                  <FileTypeIcon contentType={row.mimetype} filename={basename(row.path)} class="w-5 h-5" />
                {/if}
              </span>

              <div class="flex-1 min-w-0">
                <div class="flex items-center gap-2">
                  <span class="font-medium truncate">{basename(row.path)}</span>
                  {#if row.label}
                    <span
                      class="badge text-xs flex-shrink-0 preset-tonal-surface"
                      title={m.shares_view_share_label_title()}
                    >{row.label}</span>
                  {/if}
                </div>
                <p class="text-xs text-surface-500 truncate">
                  <span class="font-mono">{folder}</span>
                  <span class="mx-1">·</span>
                  <span>{formatRelative(row.stime)}</span>
                </p>
                <div class="flex items-center gap-3 mt-1 text-[11px] text-surface-500">
                  <span class="inline-flex items-center gap-1" title={m.shares_view_perm_title()}>
                    <Icon name="share-links" size={12} />
                    {permissionLabel(row.permissions)}
                  </span>
                  <span class="inline-flex items-center gap-1" title={row.has_password ? m.shares_view_password_set_title() : m.shares_view_password_none_title()}>
                    <Icon name={row.has_password ? 'lock' : 'unlocked'} size={12} />
                    {row.has_password ? m.shares_view_password_set() : m.shares_view_password_none()}
                  </span>
                  {#if row.expiration}
                    <span class="inline-flex items-center gap-1" title={m.shares_view_expires_title()}>
                      <Icon name="time" size={12} />
                      {m.shares_view_expires_label({ date: formatExpiryDate(row.expiration) })}
                    </span>
                  {:else}
                    <span class="inline-flex items-center gap-1" title={m.shares_view_no_expiry_title()}>
                      <Icon name="time" size={12} />
                      {m.shares_view_no_expiry_label()}
                    </span>
                  {/if}
                </div>
              </div>

              <!-- Per-row icon-only action buttons.  Class string
                   matches the canonical pattern in CLAUDE.md so
                   every row's set of actions reads as siblings of
                   the same shape; only the destructive (revoke)
                   variant gets the red-on-hover overlay so it
                   stays calm at rest. -->
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                onclick={() => copyUrl(row)}
                title={copiedId === row.id ? m.shares_view_copied() : m.shares_view_copy_title()}
                aria-label={copiedId === row.id ? m.shares_view_copied() : m.shares_view_copy_link()}
              ><Icon name={copiedId === row.id ? 'success' : 'copy'} size={14} /></button>
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                onclick={() => openEdit(row)}
                title={m.shares_view_edit_title()}
                aria-label={m.shares_view_edit()}
              ><Icon name="compose" size={14} /></button>
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                onclick={() => openInBrowser(row)}
                title={m.shares_view_open_title()}
                aria-label={m.shares_view_open()}
              ><Icon name="open-link" size={14} /></button>
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-error-500/15 hover:text-error-500 hover:border-error-500/40"
                disabled={deletingId === row.id}
                onclick={() => void deleteShare(row)}
                title={deletingId === row.id ? m.shares_view_revoking() : m.shares_view_revoke_title()}
                aria-label={deletingId === row.id ? m.shares_view_revoking() : m.shares_view_revoke()}
              ><Icon name={deletingId === row.id ? 'loading' : 'trash'} size={14} /></button>
            </li>
          {/each}
        </ul>
      </div>
    {/if}
  {/if}
</div>

{#if editing}
  <div
    class="fixed inset-0 flex items-center justify-center bg-black/50"
    style="z-index: 50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => {
      if (e.target === e.currentTarget && !editing_busy) cancelEdit()
    }}
  >
    <div class="glass-float rounded-2xl w-[28rem] max-w-full p-5">
      <h3 class="text-base font-semibold mb-1">{m.shares_edit_title()}</h3>
      <p class="text-xs text-surface-500 mb-3 truncate" title={editing.path}>
        {basename(editing.path)}
      </p>

      <!-- Permissions ------------------------------------------------ -->
      <label class="block text-xs text-surface-500 mb-1" for="shares-edit-perms">
        {m.shares_edit_perm_label()}
      </label>
      <select
        id="shares-edit-perms"
        class="select w-full text-sm px-2 py-1.5 rounded-lg mb-3"
        bind:value={editPermissions}
        disabled={editing_busy}
      >
        {#each PERMISSION_OPTIONS as opt (opt.value)}
          {#if opt.value <= 3 || editing.item_type === 'folder'}
            <option value={opt.value}>{opt.label()}</option>
          {/if}
        {/each}
        {#if !PERMISSION_OPTIONS.some((o) => o.value === editPermissions)}
          <!-- Surface a "Custom" option that mirrors the bitmask
               we got back from Nextcloud so a server-side advanced
               setting doesn't read as "View only" inside Unkai. -->
          <option value={editPermissions}>{m.shares_view_perm_custom({ value: String(editPermissions) })}</option>
        {/if}
      </select>

      <!-- Password --------------------------------------------------- -->
      <fieldset class="mb-3">
        <legend class="text-xs text-surface-500 mb-1">{m.shares_edit_password_legend()}</legend>
        <div class="flex flex-col gap-1.5 text-sm">
          <label class="inline-flex items-center gap-2 cursor-pointer">
            <input type="radio" class="radio" bind:group={editPasswordMode} value="keep" disabled={editing_busy} />
            <span>{editing.has_password ? m.shares_edit_password_keep_set() : m.shares_edit_password_keep_none()}</span>
          </label>
          {#if editing.has_password}
            <label class="inline-flex items-center gap-2 cursor-pointer">
              <input type="radio" class="radio" bind:group={editPasswordMode} value="clear" disabled={editing_busy} />
              <span>{m.shares_edit_password_remove()}</span>
            </label>
          {/if}
          <label class="inline-flex items-center gap-2 cursor-pointer">
            <input type="radio" class="radio" bind:group={editPasswordMode} value="replace" disabled={editing_busy} />
            <span>{editing.has_password ? m.shares_edit_password_set_new() : m.shares_edit_password_set_first()}</span>
          </label>
          {#if editPasswordMode === 'replace'}
            <PasswordInput
              class="mt-1"
              inputClass="text-sm px-2 py-1.5 rounded-lg"
              placeholder={m.shares_edit_password_placeholder()}
              bind:value={editPasswordValue}
              disabled={editing_busy}
            />
          {/if}
        </div>
      </fieldset>

      <!-- Expiry ----------------------------------------------------- -->
      <label class="flex items-center gap-2 mb-2 text-xs text-surface-700 dark:text-surface-300 cursor-pointer">
        <input
          type="checkbox"
          class="checkbox"
          bind:checked={editSetExpiration}
          disabled={editing_busy}
          onchange={() => {
            if (editSetExpiration && !editExpireDate) {
              const d = new Date()
              d.setDate(d.getDate() + 7)
              const pad = (n: number) => String(n).padStart(2, '0')
              editExpireDate = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
            }
          }}
        />
        <span>{m.shares_edit_expiration_label()}</span>
      </label>
      {#if editSetExpiration}
        <div class="mb-3">
          <DateField
            bind:value={editExpireDate}
            ariaLabel={m.shares_edit_expiration_aria()}
          />
          <p class="text-[11px] text-surface-500 mt-1">
            {m.shares_edit_expiration_hint()}
          </p>
        </div>
      {/if}

      {#if editError}
        <p class="text-xs text-error-500 mb-3 wrap-break-word">{editError}</p>
      {/if}

      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500 shrink-0"
          disabled={editing_busy}
          onclick={cancelEdit}
        >{m.shares_edit_cancel()}</button>
        <button
          class="btn preset-filled-primary-500 shrink-0 whitespace-nowrap"
          disabled={editing_busy}
          onclick={() => void commitEdit()}
        >
          {#if editing_busy}{m.shares_edit_saving()}{:else}{m.shares_edit_save()}{/if}
        </button>
      </div>
    </div>
  </div>
{/if}

<script lang="ts" module>
  /**
   * NextcloudShareDialog — the password / permissions / expiry
   * sub-modal used to mint public share links.
   *
   * Two callers today, both driving the same OCS endpoint via
   * `create_nextcloud_share`:
   *   - **NextcloudFilePicker** — opens from inside Compose's
   *     "Attach from Nextcloud" flow.  Sits over its parent's
   *     own modal so it renders at `zIndex: 70`; passes the
   *     recipient string as `shareLabel` so the Nextcloud
   *     "Shared with others" list shows who got the link
   *     (#91).
   *   - **FilesView** — opens from the sidebar-routed Files
   *     view's "New mail with link" action.  No outer modal,
   *     so the default `zIndex: 50` is fine.  No recipient
   *     list yet (Compose hasn't opened), so no `shareLabel`.
   *
   * Both surfaces previously embedded near-identical inline
   * modals; #324 added an expiry picker to the picker side
   * and revealed how much duplication had built up.  Pulling
   * the markup + state + IPC into one component keeps the
   * two flows from drifting (permission options, copy,
   * dropdown styling, and now expiry all live in one place).
   */

  /** A "share as link" result. `id` / `ncId` ride along so the
   *  caller can drive later share-label updates (#91) or
   *  share-deletion-on-draft-discard cleanup (#193).  Callers
   *  that don't need them are free to project the value down
   *  to `{filename, url}`. */
  export interface ShareLink {
    filename: string
    url: string
    id: string
    ncId: string
  }
</script>

<script lang="ts">
  import { invoke } from '@tauri-apps/api/core'
  import DateField from './DateField.svelte'
  import { formatError } from './errors'
  import { m } from '../paraglide/messages'

  interface Props {
    /** Whether the dialog is rendered.  The parent owns
     *  open/close state so it can coordinate with its own
     *  keyboard handlers (e.g. a wrapping picker's Esc-closes-
     *  outer-modal behaviour). */
    open: boolean
    /** Nextcloud account id to bill the share against. */
    accountId: string
    /** Selected paths to mint share links for.  The snapshot is
     *  taken by the parent at click time so toggling the file
     *  tree behind the dialog can't change what gets shared. */
    paths: string[]
    /** True if any of `paths` points at a folder — drives which
     *  permission options the dropdown surfaces.  "Upload + edit"
     *  and "File drop" only make sense for folder shares; file-only
     *  selections shouldn't see them. */
    hasFolders: boolean
    /** Optional human-readable label attached to every share
     *  (#91) so the Nextcloud "Shared with others" list shows
     *  who got the link rather than the auto-generated name. */
    shareLabel?: string
    /** Stacking depth.  The picker nests this dialog over its
     *  own modal so it needs `70`; the standalone Files surface
     *  has no outer modal and uses `50`. */
    zIndex?: number
    /** Called with the freshly-minted share links once the OCS
     *  calls succeed.  The parent decides what to do next
     *  (insert into draft, hand off to Compose, …) and is also
     *  responsible for hiding the dialog by clearing `open`. */
    onresolve: (links: ShareLink[]) => void
    /** Called when the user dismisses without sharing — Cancel
     *  button, Esc, or backdrop click. */
    oncancel: () => void
  }
  let {
    open,
    accountId,
    paths,
    hasFolders,
    shareLabel,
    zIndex = 50,
    onresolve,
    oncancel,
  }: Props = $props()

  // Form state.  Reset each time the dialog opens so a previous
  // run's password / permissions / expiry don't leak into the
  // next selection.
  let password = $state('')
  let permissions = $state(1)
  let setExpiration = $state(false)
  let expireDate = $state('')
  let sharing = $state(false)
  let error = $state('')

  $effect(() => {
    if (open) {
      password = ''
      permissions = 1
      setExpiration = false
      expireDate = ''
      sharing = false
      error = ''
    }
  })

  /** Common public-link permission combinations Nextcloud's own
   *  share UI exposes.  The bitfield (1 read, 2 update, 4 create,
   *  8 delete, 16 share) gets sent to the OCS endpoint verbatim. */
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

  let visiblePerms = $derived(
    PERMISSION_OPTIONS.filter((o) => !o.folderOnly || hasFolders),
  )
  function permHint(value: number): string {
    return PERMISSION_OPTIONS.find((o) => o.value === value)?.hint ?? ''
  }

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

  function basename(path: string): string {
    return path.split('/').filter(Boolean).pop() ?? path
  }

  async function commitShare() {
    if (!open || paths.length === 0) return
    sharing = true
    error = ''
    try {
      const pw = password.trim() ? password : null
      // Only send `expireDate` when the user actually opted into
      // it.  An empty `expireDate=` posted to OCS is parsed as a
      // bad date and rejects the whole share, so the toggle-off
      // path must omit the field entirely.
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
      onresolve(results)
    } catch (e) {
      error = formatError(e) || 'Failed to create share link(s)'
    } finally {
      sharing = false
    }
  }
</script>

{#if open}
  <div
    class="fixed inset-0 flex items-center justify-center bg-black/50"
    style="z-index: {zIndex}"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => {
      if (e.target === e.currentTarget && !sharing) oncancel()
    }}
  >
    <div class="glass-float rounded-2xl w-[28rem] max-w-full p-5">
      <h3 class="text-base font-semibold mb-1">Password-protect link?</h3>
      <p class="text-xs text-surface-500 mb-3">
        {paths.length === 1
          ? 'Anyone with the link can open the file.'
          : `Anyone with each link can open ${paths.length} files.`}
        Setting a password gates the recipient behind it; leave it empty
        to share without one.
      </p>

      <label class="block text-xs text-surface-500 mb-1" for="nc-share-dialog-pw">
        Password (optional)
      </label>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        id="nc-share-dialog-pw"
        type="password"
        class="input w-full text-sm px-2 py-1.5 rounded-lg mb-3"
        placeholder="Leave blank for no password"
        bind:value={password}
        disabled={sharing}
        autofocus
        onkeydown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault()
            void commitShare()
          } else if (e.key === 'Escape' && !sharing) {
            e.preventDefault()
            oncancel()
          }
        }}
      />

      <!-- Permissions dropdown — mirrors Nextcloud's own share UI.
           The bitmask values map to OCS's `permissions` form field.
           File-only flows (where the picker is used to attach a
           single document) practically only use 1 / 3; the upload
           variants ride along for folder shares. -->
      <label class="block text-xs text-surface-500 mb-1" for="nc-share-dialog-perms">
        Permissions
      </label>
      <select
        id="nc-share-dialog-perms"
        class="select w-full text-sm px-2 py-1.5 rounded-lg mb-1"
        bind:value={permissions}
        disabled={sharing}
      >
        {#each visiblePerms as opt (opt.value)}
          <option value={opt.value}>{opt.label}</option>
        {/each}
      </select>
      <p class="text-[11px] text-surface-500 mb-3">
        {permHint(permissions)}
      </p>

      <!-- Expiration date (#324).  Nextcloud's OCS endpoint takes an
           optional `expireDate=YYYY-MM-DD`; after that date the
           public link returns a "Link expired" page.  Gated behind
           a checkbox so the no-expiry default stays one click away,
           matching Nextcloud's own UI. -->
      <label class="flex items-center gap-2 mb-2 text-xs text-surface-700 dark:text-surface-300 cursor-pointer">
        <input
          type="checkbox"
          class="checkbox"
          bind:checked={setExpiration}
          disabled={sharing}
          onchange={() => {
            // Seed a sensible default the first time the user opts
            // in, otherwise reopening the toggle would land on an
            // empty field that DateField renders as "Pick a date".
            if (setExpiration && !expireDate) expireDate = defaultExpiry()
          }}
        />
        <span>{m.nc_share_set_expiration_label()}</span>
      </label>
      {#if setExpiration}
        <div class="mb-3">
          <DateField
            bind:value={expireDate}
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
          onclick={oncancel}
        >
          Cancel
        </button>
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

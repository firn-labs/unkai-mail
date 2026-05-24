<!--
  Per-account end-to-end encryption settings (#57).

  Mounted inside the existing AccountSettings.svelte per-account
  expandable block.  Two states:

  - **No key imported** — show an "Import OpenPGP private key" affordance
    that takes a pasted armored block + passphrase, validates via the
    `pgp_import_private_key` IPC, and refreshes the panel.

  - **Key imported** — show the fingerprint plus a "Remove" button that
    clears the keychain entry and the cached fingerprint on the
    account row.

  Per the #57 design decision, the passphrase is **not** persisted
  after import.  The Compose / MailView surfaces re-prompt for it on
  every encrypt / decrypt operation.  The import flow uses the
  passphrase only to prove the key actually decrypts before we
  accept it.

  The component is deliberately compact — no fancy file picker yet,
  paste-from-clipboard is the only entry path.  A future PR can
  add a tauri-plugin-dialog file picker over the same Tauri command.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core'
  import { m } from '../paraglide/messages'
  import { forgetPassphrase } from './sessionPassphraseStore.svelte'

  /** Status payload returned by `pgp_get_account_key_status`. */
  interface PgpKeyStatus {
    has_key: boolean
    fingerprint: string | null
  }

  /** Minimal account shape we need — id is the only required field
   *  in this component, the rest is just for display. */
  interface AccountLite {
    id: string
    email: string
  }

  // ── Props ─────────────────────────────────────────────────────
  let { account, oncrypto_changed } = $props<{
    account: AccountLite
    /** Fires after a successful import or remove so the parent can
     *  refresh its `accounts` list and pick up the new
     *  `pgp_key_fingerprint` field. */
    oncrypto_changed?: () => void
  }>()

  // ── Local state ───────────────────────────────────────────────
  /** Cached status; loaded on mount and refreshed after any IPC. */
  let status = $state<PgpKeyStatus | null>(null)
  /** Toggle between read-only view and the paste-import form. */
  let importing = $state(false)
  /** Pasted armored OpenPGP private key block. */
  let armoredKeyInput = $state('')
  /** Passphrase that unlocks the pasted key.  Cleared on success
   *  *and* on cancel so it never lingers in the DOM after the
   *  modal closes. */
  let passphraseInput = $state('')
  /** Inline error surfaced under the form when the IPC fails
   *  (most often: "PGP key import failed" when the passphrase is
   *  wrong). */
  let errorMessage = $state<string | null>(null)
  /** `true` while the IPC is in flight — disables the submit
   *  button so a double-click can't enqueue two imports. */
  let busy = $state(false)

  // ── Lifecycle ─────────────────────────────────────────────────
  $effect(() => {
    // Re-fetch status whenever the active account changes.  The
    // outer AccountSettings remounts the encryption section per
    // expanded row, but this is defensive in case it ever moves
    // to a stable mount.
    void refreshStatus(account.id)
  })

  async function refreshStatus(accountId: string) {
    try {
      status = await invoke<PgpKeyStatus>('pgp_get_account_key_status', {
        accountId,
      })
    } catch (e) {
      console.warn('pgp_get_account_key_status failed', e)
      status = { has_key: false, fingerprint: null }
    }
  }

  function startImport() {
    armoredKeyInput = ''
    passphraseInput = ''
    errorMessage = null
    importing = true
  }

  function cancelImport() {
    armoredKeyInput = ''
    passphraseInput = ''
    errorMessage = null
    importing = false
  }

  async function submitImport() {
    if (!armoredKeyInput.trim() || busy) {
      return
    }
    busy = true
    errorMessage = null
    try {
      await invoke<string>('pgp_import_private_key', {
        accountId: account.id,
        armoredKey: armoredKeyInput,
        passphrase: passphraseInput,
      })
      armoredKeyInput = ''
      passphraseInput = ''
      importing = false
      await refreshStatus(account.id)
      oncrypto_changed?.()
    } catch (e) {
      errorMessage = String(e)
    } finally {
      busy = false
    }
  }

  async function removeKey() {
    if (busy) return
    // No confirm modal here on purpose — re-importing is cheap
    // (one paste + passphrase) and a confirm dialog every time
    // adds friction to the rare "remove" case.  If users ever
    // delete by mistake, a quick re-paste restores the same
    // fingerprint.
    busy = true
    errorMessage = null
    try {
      await invoke<void>('pgp_remove_private_key', { accountId: account.id })
      // #341 — without a key there's nothing to unlock, so any
      // cached session passphrase for this account is now
      // meaningless.  Drop it so a fresh import (possibly with
      // a different passphrase) doesn't silently inherit the
      // wrong cached value.
      forgetPassphrase(account.id)
      await refreshStatus(account.id)
      oncrypto_changed?.()
    } catch (e) {
      errorMessage = String(e)
    } finally {
      busy = false
    }
  }

  /** Pretty-format a 40-char fingerprint into four-char groups
   *  for readability — matches what GnuPG and most mail UIs do
   *  in their settings panes. */
  function formatFingerprint(fp: string): string {
    return fp.match(/.{1,4}/g)?.join(' ') ?? fp
  }
</script>

<div
  class="mt-4 pt-4 border-t border-surface-200 dark:border-surface-700"
  data-test="encryption-settings"
>
  <div class="flex items-center justify-between mb-2">
    <span class="text-sm font-medium">{m.encryption_section_title()}</span>
  </div>

  {#if status === null}
    <div class="text-xs text-surface-400">{m.encryption_status_loading()}</div>
  {:else if status.has_key && !importing}
    <!-- Key present: fingerprint + remove. -->
    <div class="space-y-2">
      <div class="text-xs text-surface-500">
        {m.encryption_active_key_label()}
      </div>
      <div class="font-mono text-xs break-all p-2 rounded bg-surface-100 dark:bg-surface-800">
        {status.fingerprint ? formatFingerprint(status.fingerprint) : '—'}
      </div>
      <div class="flex gap-2">
        <button
          type="button"
          class="btn btn-sm preset-tonal-error"
          onclick={removeKey}
          disabled={busy}
        >
          {m.encryption_remove_button()}
        </button>
        <button
          type="button"
          class="btn btn-sm preset-tonal-surface"
          onclick={startImport}
          disabled={busy}
        >
          {m.encryption_replace_button()}
        </button>
      </div>
    </div>
  {:else if importing}
    <!-- Import paste form. -->
    <div class="space-y-2">
      <label class="block text-xs text-surface-500" for="pgp-paste-{account.id}">
        {m.encryption_paste_label()}
      </label>
      <textarea
        id="pgp-paste-{account.id}"
        class="textarea font-mono text-xs"
        rows="6"
        placeholder="-----BEGIN PGP PRIVATE KEY BLOCK-----"
        bind:value={armoredKeyInput}
        disabled={busy}
      ></textarea>
      <label class="block text-xs text-surface-500" for="pgp-pw-{account.id}">
        {m.encryption_passphrase_label()}
      </label>
      <input
        id="pgp-pw-{account.id}"
        type="password"
        class="input text-xs"
        placeholder={m.encryption_passphrase_placeholder()}
        bind:value={passphraseInput}
        disabled={busy}
        autocomplete="off"
      />
      {#if errorMessage}
        <div class="text-xs text-error-500">{errorMessage}</div>
      {/if}
      <div class="flex gap-2">
        <button
          type="button"
          class="btn btn-sm preset-filled-primary"
          onclick={submitImport}
          disabled={busy || !armoredKeyInput.trim()}
        >
          {m.encryption_import_submit()}
        </button>
        <button
          type="button"
          class="btn btn-sm preset-tonal-surface"
          onclick={cancelImport}
          disabled={busy}
        >
          {m.encryption_import_cancel()}
        </button>
      </div>
      <div class="text-xs text-surface-400">
        {m.encryption_passphrase_hint()}
      </div>
    </div>
  {:else}
    <!-- No key: invite the user to import. -->
    <div class="space-y-2">
      <div class="text-xs text-surface-500">
        {m.encryption_no_key_explanation()}
      </div>
      <button
        type="button"
        class="btn btn-sm preset-tonal-primary"
        onclick={startImport}
      >
        {m.encryption_import_button()}
      </button>
    </div>
  {/if}
</div>

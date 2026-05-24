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
  import Toggle from './Toggle.svelte'
  import Icon from './Icon.svelte'
  import { formatError } from './errors'

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

  // ── #341 — "Unlock automatically" (per-account passphrase opt-in)
  /** Mirrors the keychain entry for `unkai-mail-pgp-passphrase`:
   *  `true` once the user has opted in, `false` otherwise.  Loaded
   *  via `pgp_has_unlock_automatically` on mount and re-fetched
   *  after every enable / disable so the toggle visual reflects
   *  the actual keychain state (not just the user's intent). */
  let unlockEnabled = $state(false)
  /** UI state for the "user flipped the toggle ON, now type a
   *  passphrase to save" inline form.  Distinct from
   *  `unlockEnabled` because we don't flip that to true until the
   *  IPC succeeds — wrong-passphrase saves shouldn't visually
   *  enable a feature that isn't really on. */
  let unlockEnabling = $state(false)
  let unlockPassphrase = $state('')
  let unlockError = $state<string | null>(null)
  let unlockBusy = $state(false)

  async function refreshUnlockStatus(accountId: string) {
    try {
      unlockEnabled = await invoke<boolean>('pgp_has_unlock_automatically', {
        accountId,
      })
    } catch (e) {
      // Treat a keychain query error as "off" so a momentary OS
      // hiccup doesn't surface as a misleadingly-on toggle.  The
      // user's next enable attempt will surface the real error.
      console.warn('pgp_has_unlock_automatically failed', e)
      unlockEnabled = false
    }
  }

  function startEnablingUnlock() {
    unlockPassphrase = ''
    unlockError = null
    unlockEnabling = true
  }

  function cancelEnablingUnlock() {
    unlockPassphrase = ''
    unlockError = null
    unlockEnabling = false
  }

  async function submitEnableUnlock() {
    if (!unlockPassphrase || unlockBusy) return
    unlockBusy = true
    unlockError = null
    try {
      await invoke('pgp_enable_unlock_automatically', {
        accountId: account.id,
        passphrase: unlockPassphrase,
      })
      unlockPassphrase = ''
      unlockEnabling = false
      await refreshUnlockStatus(account.id)
    } catch (e) {
      const raw = formatError(e) || 'Failed to enable Unlock automatically'
      // Strip the typed-enum prefix so wrong-passphrase reads as a
      // clean sentence — same idiom MailView uses for `runDecrypt`.
      unlockError = raw.replace(/^Crypto:\s*/i, '')
    } finally {
      unlockBusy = false
    }
  }

  async function disableUnlock() {
    if (unlockBusy) return
    unlockBusy = true
    unlockError = null
    try {
      await invoke('pgp_disable_unlock_automatically', {
        accountId: account.id,
      })
      await refreshUnlockStatus(account.id)
    } catch (e) {
      unlockError = formatError(e) || 'Failed to disable Unlock automatically'
    } finally {
      unlockBusy = false
    }
  }

  /** Toggle handler — flips ON opens the inline passphrase entry;
   *  flips OFF immediately drops the keychain entry.  No confirm
   *  dialog on disable because re-enabling is cheap (one typed
   *  passphrase) and a confirm here would add friction to a setting
   *  whose default is OFF anyway. */
  function onUnlockToggle(next: boolean) {
    if (next) {
      startEnablingUnlock()
    } else {
      // Cancel any half-finished enable flow if the user toggles
      // off mid-typing — the inline entry should disappear
      // immediately rather than wait for an unrelated IPC.
      unlockEnabling = false
      unlockPassphrase = ''
      unlockError = null
      void disableUnlock()
    }
  }

  // ── Lifecycle ─────────────────────────────────────────────────
  $effect(() => {
    // Re-fetch status whenever the active account changes.  The
    // outer AccountSettings remounts the encryption section per
    // expanded row, but this is defensive in case it ever moves
    // to a stable mount.
    void refreshStatus(account.id)
    void refreshUnlockStatus(account.id)
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
      // #341 — `pgp_remove_private_key` already drops the passphrase
      // keychain entry defensively (it has since #57), but the UI's
      // mirror of that state needs a refresh so the toggle visual
      // catches up to the keychain reality.
      await refreshStatus(account.id)
      await refreshUnlockStatus(account.id)
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
  <!-- Title row doubles as the per-key action surface: Replace
       (compose / edit) and Remove (trash, red on hover) sit
       inline with the section header on the right.  Gated on
       `has_key && !importing` so the affordances only appear
       when there's actually a key to act on; the no-key +
       importing states render their own primary CTAs lower in
       the block. -->
  <div class="flex items-center justify-between mb-2">
    <span class="text-sm font-medium">{m.encryption_section_title()}</span>
    {#if status?.has_key && !importing}
      <div class="flex gap-2">
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
          onclick={startImport}
          disabled={busy}
          title={m.encryption_replace_button()}
          aria-label={m.encryption_replace_button()}
        >
          <Icon name="compose" size={14} />
        </button>
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
          onclick={removeKey}
          disabled={busy}
          title={m.encryption_remove_button()}
          aria-label={m.encryption_remove_button()}
        >
          <Icon name="trash" size={14} />
        </button>
      </div>
    {/if}
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
      <!-- #341 — Per-account "Unlock automatically" opt-in.  Sits
           below the key fingerprint because it's only meaningful
           when a key is imported.  Off by default; flipping it on
           drops a passphrase into the OS keychain so Compose,
           MailView, Reply/Forward, and the receive path all
           auto-supply it without re-prompting.

           Layout: toggle on the left of the label (the project's
           default toggle-row shape), with the saved badge or
           off-state hint promoted to its own flush-left line
           below the row rather than living in the column to the
           right of the toggle — otherwise the badge / hint reads
           as visually indented under the toggle. -->
      <div class="mt-3 pt-3 border-t border-surface-200 dark:border-surface-700">
        <div class="flex items-center gap-3">
          <Toggle
            checked={unlockEnabled || unlockEnabling}
            disabled={unlockBusy}
            onchange={onUnlockToggle}
            label={m.encryption_unlock_auto_label()}
          />
          <p class="text-sm font-medium leading-tight">
            {m.encryption_unlock_auto_label()}
          </p>
        </div>
        {#if unlockEnabled}
          <!-- Saved-state badge: explicit visual confirmation
               that the passphrase is on file in the keychain,
               so the user doesn't have to infer it from the
               toggle position alone. -->
          <div
            class="inline-flex items-center gap-1 mt-1 text-xs text-success-500"
            aria-live="polite"
          >
            <Icon name="success" size={14} />
            <span>{m.encryption_unlock_auto_saved()}</span>
          </div>
        {:else}
          <p class="text-xs text-surface-500 mt-1 leading-snug">
            {m.encryption_unlock_auto_hint_off()}
          </p>
        {/if}

        {#if unlockEnabling}
          <div class="mt-3 space-y-2">
            <!-- No standalone "Passphrase" label: the placeholder
                 ("Passphrase for your PGP key") doubles as the
                 prompt while keeping the form compact.  Visible
                 input border carries the "type here" signal. -->
            <input
              id="pgp-unlock-pw-{account.id}"
              type="password"
              class="input text-xs border-2 border-surface-400 dark:border-surface-500 focus:border-primary-500 dark:focus:border-primary-500"
              placeholder={m.encryption_unlock_auto_passphrase_placeholder()}
              aria-label={m.encryption_unlock_auto_passphrase_label()}
              bind:value={unlockPassphrase}
              disabled={unlockBusy}
              autocomplete="off"
              onkeydown={(e) => {
                if (e.key === 'Enter' && unlockPassphrase) {
                  e.preventDefault()
                  void submitEnableUnlock()
                }
              }}
            />
            {#if unlockError}
              <div class="text-xs text-error-500">{unlockError}</div>
            {/if}
            <div class="flex gap-2">
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                onclick={submitEnableUnlock}
                disabled={unlockBusy || !unlockPassphrase}
                title={m.encryption_unlock_auto_save()}
                aria-label={m.encryption_unlock_auto_save()}
              >
                <Icon name="save-draft" size={14} />
              </button>
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                onclick={cancelEnablingUnlock}
                disabled={unlockBusy}
                title={m.encryption_unlock_auto_cancel()}
                aria-label={m.encryption_unlock_auto_cancel()}
              >
                <Icon name="close" size={14} />
              </button>
            </div>
            <p class="text-xs text-surface-400">
              {m.encryption_unlock_auto_storage_hint()}
            </p>
          </div>
        {/if}
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
        class="input text-xs border-2 border-surface-400 dark:border-surface-500 focus:border-primary-500 dark:focus:border-primary-500"
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

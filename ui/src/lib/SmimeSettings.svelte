<!--
  Per-account S/MIME (X.509) certificate settings (#338, Chunk 6).

  Sibling to EncryptionSettings.svelte (the OpenPGP panel) — mounted
  right below it inside AccountSettings.svelte's per-account block.
  Two states:

  - **No certificate imported** — show an "Import certificate" affordance
    that takes a `.p12` / `.pfx` file picked from disk + its passphrase,
    validates via the `smime_import_pkcs12` IPC, and refreshes the panel.

  - **Certificate imported** — show the SHA-256 fingerprint plus a
    "Remove" button that clears the keychain entry and the cached
    fingerprint on the account row.

  Unlike the OpenPGP panel, the private identity is a binary PKCS#12
  bundle, so the entry path is a native file picker (an HTML
  `<input type="file">` read through `FileReader` and base64-encoded
  for the IPC) rather than a paste box.  As with OpenPGP, the
  passphrase is **not** persisted after import unless the user opts
  into "Unlock automatically".
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core'
  import { m } from '../paraglide/messages'
  import Toggle from './Toggle.svelte'
  import Icon from './Icon.svelte'
  import { formatError } from './errors'

  /** Status payload returned by `smime_get_account_cert_status`. */
  interface SmimeCertStatus {
    has_cert: boolean
    fingerprint: string | null
  }

  interface AccountLite {
    id: string
    email: string
  }

  // ── Props ─────────────────────────────────────────────────────
  let { account, oncrypto_changed } = $props<{
    account: AccountLite
    /** Fires after a successful import or remove so the parent can
     *  refresh its `accounts` list and pick up the new
     *  `smime_cert_fingerprint` field. */
    oncrypto_changed?: () => void
  }>()

  // ── Local state ───────────────────────────────────────────────
  let status = $state<SmimeCertStatus | null>(null)
  /** Toggle between read-only view and the file-import form. */
  let importing = $state(false)
  /** The `.p12` / `.pfx` file the user picked, read to base64 only
   *  at submit time so the bytes don't linger longer than needed. */
  let selectedFile = $state<File | null>(null)
  /** Passphrase that unlocks the chosen bundle.  Cleared on success
   *  *and* on cancel so it never lingers in the DOM after the form
   *  closes. */
  let passphraseInput = $state('')
  let errorMessage = $state<string | null>(null)
  let busy = $state(false)
  /** Bound to the hidden `<input type="file">` so the visible button
   *  can trigger the native picker. */
  let fileInput = $state<HTMLInputElement | null>(null)

  // ── "Unlock automatically" (per-account passphrase opt-in) ─────
  // Mirrors the OpenPGP panel exactly — see EncryptionSettings.svelte
  // for the rationale behind the `unlockEnabled` vs `unlockEnabling`
  // split.
  let unlockEnabled = $state(false)
  let unlockEnabling = $state(false)
  let unlockPassphrase = $state('')
  let unlockError = $state<string | null>(null)
  let unlockBusy = $state(false)

  async function refreshUnlockStatus(accountId: string) {
    try {
      unlockEnabled = await invoke<boolean>('smime_has_unlock_automatically', {
        accountId,
      })
    } catch (e) {
      console.warn('smime_has_unlock_automatically failed', e)
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
      await invoke('smime_enable_unlock_automatically', {
        accountId: account.id,
        passphrase: unlockPassphrase,
      })
      unlockPassphrase = ''
      unlockEnabling = false
      await refreshUnlockStatus(account.id)
    } catch (e) {
      const raw = formatError(e) || 'Failed to enable Unlock automatically'
      // Strip the typed-enum prefix so wrong-passphrase reads as a
      // clean sentence — same idiom the OpenPGP panel uses.
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
      await invoke('smime_disable_unlock_automatically', {
        accountId: account.id,
      })
      await refreshUnlockStatus(account.id)
    } catch (e) {
      unlockError = formatError(e) || 'Failed to disable Unlock automatically'
    } finally {
      unlockBusy = false
    }
  }

  function onUnlockToggle(next: boolean) {
    if (next) {
      startEnablingUnlock()
    } else {
      unlockEnabling = false
      unlockPassphrase = ''
      unlockError = null
      void disableUnlock()
    }
  }

  // ── Lifecycle ─────────────────────────────────────────────────
  $effect(() => {
    void refreshStatus(account.id)
    void refreshUnlockStatus(account.id)
  })

  async function refreshStatus(accountId: string) {
    try {
      status = await invoke<SmimeCertStatus>('smime_get_account_cert_status', {
        accountId,
      })
    } catch (e) {
      console.warn('smime_get_account_cert_status failed', e)
      status = { has_cert: false, fingerprint: null }
    }
  }

  function startImport() {
    selectedFile = null
    passphraseInput = ''
    errorMessage = null
    importing = true
  }

  function cancelImport() {
    selectedFile = null
    passphraseInput = ''
    errorMessage = null
    importing = false
  }

  function onFileChosen(e: Event) {
    const input = e.currentTarget as HTMLInputElement
    selectedFile = input.files?.[0] ?? null
    errorMessage = null
  }

  /** Read a picked file to a bare base64 string.  Routes through
   *  `FileReader.readAsDataURL` (then strips the `data:…;base64,`
   *  prefix) rather than `btoa(String.fromCharCode(...))` so a large
   *  bundle can't blow the apply-size limit — same technique Compose
   *  uses for attachment bytes. */
  function fileToBase64(file: File): Promise<string> {
    return new Promise((resolve, reject) => {
      const r = new FileReader()
      r.onload = () => {
        const result = r.result as string
        const comma = result.indexOf(',')
        resolve(comma >= 0 ? result.slice(comma + 1) : result)
      }
      r.onerror = () => reject(r.error)
      r.readAsDataURL(file)
    })
  }

  async function submitImport() {
    if (!selectedFile || busy) {
      return
    }
    busy = true
    errorMessage = null
    try {
      const pkcs12Base64 = await fileToBase64(selectedFile)
      await invoke<string>('smime_import_pkcs12', {
        accountId: account.id,
        pkcs12Base64,
        passphrase: passphraseInput,
      })
      selectedFile = null
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

  async function removeCert() {
    if (busy) return
    busy = true
    errorMessage = null
    try {
      await invoke<void>('smime_remove_private_cert', { accountId: account.id })
      // `smime_remove_private_cert` already drops the passphrase
      // keychain entry, but the UI's mirror of that state needs a
      // refresh so the toggle visual catches up.
      await refreshStatus(account.id)
      await refreshUnlockStatus(account.id)
      oncrypto_changed?.()
    } catch (e) {
      errorMessage = String(e)
    } finally {
      busy = false
    }
  }
</script>

<div
  class="mt-4 pt-4 border-t border-surface-200 dark:border-surface-700"
  data-test="smime-settings"
>
  <div class="flex items-center justify-between mb-2">
    <span class="text-sm font-medium">{m.smime_section_title()}</span>
    {#if status?.has_cert && !importing}
      <div class="flex gap-2">
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
          onclick={startImport}
          disabled={busy}
          title={m.smime_replace_button()}
          aria-label={m.smime_replace_button()}
        >
          <Icon name="compose" size={14} />
        </button>
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
          onclick={removeCert}
          disabled={busy}
          title={m.smime_remove_button()}
          aria-label={m.smime_remove_button()}
        >
          <Icon name="trash" size={14} />
        </button>
      </div>
    {/if}
  </div>

  {#if status === null}
    <div class="text-xs text-surface-400">{m.smime_status_loading()}</div>
  {:else if status.has_cert && !importing}
    <!-- Certificate present: fingerprint + remove. -->
    <div class="space-y-2">
      <div class="text-xs text-surface-500">
        {m.smime_active_cert_label()}
      </div>
      <!-- The S/MIME fingerprint is already the colon-separated
           SHA-256 form (the universal X.509 display convention), so
           it's rendered verbatim — no four-char regrouping like the
           OpenPGP panel does. -->
      <div class="font-mono text-xs break-all p-2 rounded bg-surface-100 dark:bg-surface-800">
        {status.fingerprint ?? '—'}
      </div>
      <!-- Per-account "Unlock automatically" opt-in.  Sits below the
           fingerprint because it's only meaningful when a cert is
           imported.  Off by default; flipping it on drops a
           passphrase into the OS keychain so Compose and the receive
           path auto-supply it without re-prompting. -->
      <div class="mt-3 pt-3 border-t border-surface-200 dark:border-surface-700">
        <div class="flex items-start gap-3">
          <Toggle
            checked={unlockEnabled || unlockEnabling}
            disabled={unlockBusy}
            onchange={onUnlockToggle}
            label={m.smime_unlock_auto_label()}
            class="mt-0.5"
          />
          <div class="flex-1 min-w-0">
            <p class="text-sm font-medium leading-tight">
              {m.smime_unlock_auto_label()}
            </p>
            {#if unlockEnabled}
              <div
                class="inline-flex items-center gap-1 mt-1 text-xs text-success-500"
                aria-live="polite"
              >
                <Icon name="success" size={14} />
                <span>{m.smime_unlock_auto_saved()}</span>
              </div>
            {:else}
              <p class="text-xs text-surface-500 mt-1 leading-snug">
                {m.smime_unlock_auto_hint_off()}
              </p>
            {/if}

            {#if unlockEnabling}
              <div class="mt-3 space-y-2">
                <input
                  id="smime-unlock-pw-{account.id}"
                  type="password"
                  class="input flex-1 text-sm px-2 py-1 rounded-lg"
                  placeholder={m.smime_unlock_auto_passphrase_placeholder()}
                  aria-label={m.smime_unlock_auto_passphrase_label()}
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
                    title={m.smime_unlock_auto_save()}
                    aria-label={m.smime_unlock_auto_save()}
                  >
                    <Icon name="save-draft" size={14} />
                  </button>
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                    onclick={cancelEnablingUnlock}
                    disabled={unlockBusy}
                    title={m.smime_unlock_auto_cancel()}
                    aria-label={m.smime_unlock_auto_cancel()}
                  >
                    <Icon name="close" size={14} />
                  </button>
                </div>
                <p class="text-xs text-surface-400">
                  {m.smime_unlock_auto_storage_hint()}
                </p>
              </div>
            {/if}
          </div>
        </div>
      </div>
    </div>
  {:else if importing}
    <!-- Import file-picker form. -->
    <div class="space-y-2">
      <!-- Hidden native picker; the visible button triggers it so the
           control matches the panel's button vocabulary rather than a
           raw browser file input. -->
      <input
        bind:this={fileInput}
        type="file"
        accept=".p12,.pfx,application/x-pkcs12"
        class="hidden"
        onchange={onFileChosen}
      />
      <div class="flex items-center gap-2">
        <button
          type="button"
          class="btn btn-sm preset-tonal-surface"
          onclick={() => fileInput?.click()}
          disabled={busy}
        >
          {m.smime_file_choose_button()}
        </button>
        <span class="text-xs text-surface-500 break-all min-w-0 truncate">
          {selectedFile?.name ?? m.smime_file_none_selected()}
        </span>
      </div>
      <label class="block text-xs text-surface-500" for="smime-pw-{account.id}">
        {m.smime_passphrase_label()}
      </label>
      <input
        id="smime-pw-{account.id}"
        type="password"
        class="input text-xs border-2 border-surface-400 dark:border-surface-500 focus:border-primary-500 dark:focus:border-primary-500"
        placeholder={m.smime_passphrase_placeholder()}
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
          disabled={busy || !selectedFile}
        >
          {m.smime_import_submit()}
        </button>
        <button
          type="button"
          class="btn btn-sm preset-tonal-surface"
          onclick={cancelImport}
          disabled={busy}
        >
          {m.smime_import_cancel()}
        </button>
      </div>
      <div class="text-xs text-surface-400">
        {m.smime_passphrase_hint()}
      </div>
    </div>
  {:else}
    <!-- No certificate: invite the user to import. -->
    <div class="space-y-2">
      <div class="text-xs text-surface-500">
        {m.smime_no_cert_explanation()}
      </div>
      <button
        type="button"
        class="btn btn-sm preset-tonal-primary"
        onclick={startImport}
      >
        {m.smime_import_button()}
      </button>
    </div>
  {/if}
</div>

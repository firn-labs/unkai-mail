<script lang="ts">
  /**
   * Security settings — FIDO unlock enrollment (#164, Phase 1A).
   *
   * Lists registered FIDO credentials for the SQLCipher database
   * and lets the user add or remove them.  Enrollment uses
   * WebAuthn's PRF extension via the OS — Touch ID, Windows
   * Hello, USB hardware key, whichever the user picks at the
   * authenticator sheet.  The PRF output gets shipped to Rust,
   * which uses it as an AES-256-GCM key to wrap the DB master
   * key inside the keychain envelope.
   *
   * Phase 1A: enrollment scaffolding.  The plain master key
   * stays in the keychain alongside the wraps so cold launch
   * still works without a hardware-key tap.  Phase 1B will
   * delete the plain key, gate startup on unlock, and surface
   * a real lock screen.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { formatError } from './errors'
  import Icon from './Icon.svelte'
  import {
    enrollFidoCredential,
    evaluateFidoPrf,
    isWebAuthnAvailable,
  } from './webauthnPrf'
  import Toggle from './Toggle.svelte'
  import { m } from '../paraglide/messages'
  import {
    cachedPassphraseCount,
    forgetAllPassphrases,
    isSessionPassphraseEnabled,
    setSessionPassphraseEnabled,
  } from './sessionPassphraseStore.svelte'

  interface FidoCredential {
    kind: 'fido_prf' | 'passphrase'
    credentialId: string
    label: string
    salt: string
    createdAt: number
  }
  interface FidoStatus {
    hasPlainKey: boolean
    credentials: FidoCredential[]
  }

  /** Whether the registered methods meet the safety bar for
   *  enabling FIDO-only mode: at least one passphrase OR ≥ 2
   *  hardware keys.  Without one of those, losing a single key
   *  would lock the cache permanently. */
  function safeForFidoOnlyMode(s: FidoStatus | null): boolean {
    if (!s) return false
    const passphraseCount = s.credentials.filter((c) => c.kind === 'passphrase').length
    const fidoCount = s.credentials.filter((c) => c.kind === 'fido_prf').length
    return passphraseCount >= 1 || fidoCount >= 2
  }

  let status = $state<FidoStatus | null>(null)
  let loading = $state(true)
  // Per-action in-flight flags.  A single shared `busy` flag would
  // make every button's "Working…" label flip on at the same time,
  // even when only one action is actually running — so each
  // mutating handler owns its own flag, and `anyBusy` is the
  // derived "block other actions while something is in flight"
  // gate.  Buttons read their own flag for the label, and
  // `anyBusy` for `disabled` so the user can't kick off two
  // mutations concurrently.
  let addingKey = $state(false)
  let savingPassphrase = $state(false)
  let removingKey = $state(false)
  let disablingEncryption = $state(false)
  const anyBusy = $derived(addingKey || savingPassphrase || removingKey || disablingEncryption)
  let error = $state('')
  // Just a coarse "WebAuthn API exists" check.  Don't gate on
  // PRF capability up front — older engines mis-report it; let
  // the actual `credentials.create` reveal the truth.
  const webauthnAvailable = $state(isWebAuthnAvailable())

  let newLabel = $state('')
  /** Fixed label for the passphrase entry — there's only ever
   *  one passphrase wrap (it's the recovery slot, not a per-
   *  device thing), so the user doesn't need to name it. */
  const PASSPHRASE_LABEL = 'Recovery passphrase'
  let passphraseValue = $state('')
  let passphraseConfirm = $state('')
  /** Editing a passphrase that's already registered.  When false
   *  and a passphrase exists, the form collapses to a "Change
   *  passphrase" affordance on the registered-methods row. */
  let passphraseEditing = $state(false)
  /** Has a passphrase already been enrolled? */
  const hasPassphrase = $derived(
    !!status?.credentials.some((c) => c.kind === 'passphrase'),
  )
  /** Master "Key Encryption" toggle.  Drives both the
   *  registration UI gate AND the backend FIDO-only-mode
   *  switch:
   *
   *  - User flips ON: registration UI becomes interactive.
   *    As soon as the registered methods cover a recovery
   *    path (≥ 1 passphrase or ≥ 2 hardware keys), the
   *    backend automatically drops the plain master key from
   *    the keychain envelope — every subsequent cold launch
   *    will require authentication.
   *  - User flips OFF: registration UI greys out.  If the
   *    backend is already in FIDO-only mode, we can't
   *    transparently restore the plain key (we'd need to
   *    re-derive the master via the unlock flow), so we
   *    surface that as a clear error and leave the toggle
   *    on until the user does the disable dance manually.
   *
   *  Persisted in localStorage so the user's intent survives
   *  launches even when no methods are registered yet
   *  (toggle stays "on" but the backend stays plain-mode
   *  until they enroll a method). */
  let keyEncryptionEnabled = $state(false)
  $effect(() => {
    try {
      keyEncryptionEnabled = localStorage.getItem('unkai.keyEncryption') === '1'
    } catch {
      /* localStorage may be unavailable in some webview modes */
    }
  })
  function persistToggle(v: boolean) {
    try {
      localStorage.setItem('unkai.keyEncryption', v ? '1' : '0')
    } catch {
      /* swallow — webview may not expose localStorage */
    }
  }
  async function setKeyEncryption(v: boolean) {
    if (anyBusy) return
    if (v) {
      keyEncryptionEnabled = true
      persistToggle(true)
      // The backend flip happens in the $effect below once the
      // user has actually registered a recovery method.
    } else {
      // Turning off: if FIDO-only mode is active, ask the
      // backend to write the in-memory master key back into the
      // keychain envelope so cold launches stop showing the
      // lock screen.  Cache must be unlocked (it is — the user
      // is in Settings, which lives behind the unlock screen)
      // for the master key to be in memory.
      if (status && !status.hasPlainKey) {
        disablingEncryption = true
        try {
          await invoke('disable_fido_only_mode')
          keyEncryptionEnabled = false
          persistToggle(false)
          await loadStatus()
        } catch (e) {
          error = formatError(e) || 'Failed to disable key encryption'
        } finally {
          disablingEncryption = false
        }
        return
      }
      keyEncryptionEnabled = false
      persistToggle(false)
    }
  }
  /** Auto-activate FIDO-only mode when the toggle is on AND
   *  the registered methods cover a recovery path AND the
   *  backend still has the plain master key.  Saves the user
   *  from having to find a separate "activate" button. */
  $effect(() => {
    if (anyBusy) return
    if (!keyEncryptionEnabled) return
    if (!status) return
    if (!status.hasPlainKey) return // already FIDO-only
    if (!safeForFidoOnlyMode(status)) return
    void (async () => {
      try {
        await invoke('enable_fido_only_mode')
        await loadStatus()
      } catch (e) {
        error = formatError(e) || 'Failed to activate key encryption'
      }
    })()
  })

  async function loadStatus() {
    loading = true
    try {
      status = await invoke<FidoStatus>('fido_status')
    } catch (e) {
      error = formatError(e) || 'Failed to load FIDO status'
    } finally {
      loading = false
    }
  }

  $effect(() => {
    void loadStatus()
  })

  // ── Wipe-on-failed-authentication policy ────────────────────
  // Stored in the keychain envelope (so it survives across
  // FIDO-only / plain transitions).  `wipeMaxAttemptsRaw` is a
  // string so we can render an empty input as "unlimited".
  interface WipePolicy {
    enabled: boolean
    maxAttempts: number | null
  }
  let wipePolicy = $state<WipePolicy>({ enabled: false, maxAttempts: null })
  let wipeMaxAttemptsRaw = $state('')
  let wipeSaving = $state(false)

  async function loadWipePolicy() {
    try {
      const p = await invoke<WipePolicy>('get_wipe_policy')
      wipePolicy = p
      wipeMaxAttemptsRaw = p.maxAttempts != null ? String(p.maxAttempts) : ''
    } catch (e) {
      console.warn('get_wipe_policy failed', e)
    }
  }
  $effect(() => {
    void loadWipePolicy()
  })

  async function saveWipePolicy(next: WipePolicy) {
    wipeSaving = true
    try {
      await invoke('set_wipe_policy', {
        policy: { enabled: next.enabled, maxAttempts: next.maxAttempts },
      })
      wipePolicy = next
    } catch (e) {
      error = formatError(e) || 'Failed to save wipe policy'
    } finally {
      wipeSaving = false
    }
  }

  function onWipeToggle(checked: boolean) {
    const parsed = parseInt(wipeMaxAttemptsRaw, 10)
    void saveWipePolicy({
      enabled: checked,
      maxAttempts: checked && Number.isFinite(parsed) && parsed > 0 ? parsed : null,
    })
  }

  function onWipeMaxAttemptsChange() {
    if (!wipePolicy.enabled) return
    const parsed = parseInt(wipeMaxAttemptsRaw, 10)
    void saveWipePolicy({
      enabled: true,
      maxAttempts: Number.isFinite(parsed) && parsed > 0 ? parsed : null,
    })
  }

  async function addKey() {
    if (anyBusy) return
    const label = newLabel.trim() || 'Untitled hardware key'
    addingKey = true
    error = ''
    try {
      // Generate a fresh PRF salt server-side so it shares the
      // app's RNG and can't be influenced from the renderer.
      const saltB64 = await invoke<string>('fido_generate_salt')
      // The OS shows its own auth sheet here; we receive the
      // PRF output once the user authenticates.
      const enrolled = await enrollFidoCredential(saltB64, 'unkai-user', label)
      await invoke('fido_enroll', {
        credentialIdB64: enrolled.credentialIdB64,
        saltB64: enrolled.saltB64,
        prfOutputB64: enrolled.prfOutputB64,
        label,
      })
      newLabel = ''
      await loadStatus()
    } catch (e) {
      error = formatError(e) || 'Failed to enroll hardware key'
    } finally {
      addingKey = false
    }
  }

  async function addPassphrase() {
    if (anyBusy) return
    if (passphraseValue.length < 8) {
      error = 'Passphrase must be at least 8 characters.'
      return
    }
    if (passphraseValue !== passphraseConfirm) {
      error = "Passphrase and confirmation don't match."
      return
    }
    savingPassphrase = true
    error = ''
    try {
      await invoke('fido_enroll_passphrase', {
        passphrase: passphraseValue,
        label: PASSPHRASE_LABEL,
      })
      passphraseValue = ''
      passphraseConfirm = ''
      passphraseEditing = false
      await loadStatus()
    } catch (e) {
      error = formatError(e) || 'Failed to enroll passphrase'
    } finally {
      savingPassphrase = false
    }
  }

  async function removeKey(c: FidoCredential) {
    if (anyBusy) return
    // Pre-flight: removing the last unlock method while
    // FIDO-only mode is active would orphan the encrypted DB
    // forever.  The backend rejects this too, but catching it
    // here gives the user a message that names the actual
    // problem instead of the generic "cannot remove last
    // hardware key" text.
    if (status && !status.hasPlainKey && status.credentials.length <= 1) {
      error =
        c.kind === 'passphrase'
          ? "Can't remove the passphrase — it's your only unlock method. Add a hardware key first, or turn off Key Encryption."
          : "Can't remove this hardware key — it's your only unlock method. Add a passphrase or another hardware key first, or turn off Key Encryption."
      return
    }
    const promptLabel = c.kind === 'passphrase' ? 'Recovery passphrase' : c.label
    if (!confirm(`Remove "${promptLabel}"? You'll need to re-enroll to use it again.`))
      return
    removingKey = true
    error = ''
    try {
      // Require the user to actually still possess the key
      // before we let them drop the wrap.  Only meaningful for
      // FIDO PRF wraps — passphrase entries have no
      // authenticator to evaluate, and forcing one through
      // `evaluateFidoPrf` would surface a confusing
      // "WebAuthn unavailable" error on Linux.  Plain-key mode
      // skips the check entirely so a user with a lost key
      // can still drop the wrap.
      if (status && !status.hasPlainKey && c.kind === 'fido_prf') {
        await evaluateFidoPrf(c.credentialId, c.salt)
      }
      await invoke('fido_remove', { credentialIdB64: c.credentialId })
      await loadStatus()
    } catch (e) {
      error = formatError(e) || 'Failed to remove credential'
    } finally {
      removingKey = false
    }
  }

  function fmtDate(epoch: number): string {
    if (!epoch) return ''
    return new Date(epoch * 1000).toLocaleDateString(undefined, {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
    })
  }
</script>

<section class="space-y-6">
  <header>
    <h2 class="text-xl font-semibold">Security</h2>
    <p class="text-sm text-surface-500 mt-1 max-w-xl">
      Hardware-backed authentication for the local mail cache. Register a
      hardware key (YubiKey, Apple Touch ID, Windows Hello, …) to seal the
      cache's encryption key behind a tap or biometric.
    </p>
  </header>

  {#if loading}
    <p class="text-sm text-surface-500">Loading…</p>
  {:else}
    <!-- Master toggle.  Sits above the registration sections,
         left-aligned, rendered as a true iOS-style switch
         (track + sliding thumb) rather than a native checkbox.
         When off, every section below greys out and stops
         accepting input — clear visual signal that the feature
         is opt-in. -->
    <div class="flex items-center gap-3">
      <Toggle
        checked={keyEncryptionEnabled}
        label="Enable key encryption"
        onchange={(v) => void setKeyEncryption(v)}
      />
      <div class="max-w-xl">
        <p class="font-medium leading-tight">Key Encryption</p>
        <p class="text-xs text-surface-500 leading-snug mt-1">
          {#if !keyEncryptionEnabled}
            Off — the cache opens automatically using the plain master key
            stored in your OS keychain.  Turn on to seal the key behind a
            registered hardware key, biometric, or passphrase.
          {:else if status && !status.hasPlainKey}
            Active — your cache is sealed.  Every cold launch will ask you
            to authenticate with one of the registered methods below before
            the inbox loads.
          {:else if status && safeForFidoOnlyMode(status)}
            Activating… your registered methods will protect the cache on
            the next start.  (Activation usually completes within a moment
            of enrollment.)
          {:else}
            On — register at least one recovery method below.  As soon as
            you have one passphrase or two hardware keys registered, key
            encryption activates automatically and future launches will
            require authentication.
          {/if}
        </p>
      </div>
    </div>

    <!-- Wipe-on-failed-authentication policy.  Sits directly
         under the Key Encryption toggle so users see the
         destructive companion option while they're still
         deciding whether to enable encryption at all.  Off by
         default; when on, exposes a numeric "retries" field
         whose empty / zero value means unlimited. -->
    <div class="rounded-md border border-surface-200 dark:border-surface-700 p-4 space-y-3 {keyEncryptionEnabled ? '' : 'opacity-50 pointer-events-none select-none'}">
      <div class="flex items-start gap-3">
        <Toggle
          checked={wipePolicy.enabled}
          disabled={wipeSaving || !keyEncryptionEnabled}
          onchange={onWipeToggle}
          label="Wipe cache on failed authentication"
          class="mt-0.5"
        />
        <div>
          <h3 class="font-medium leading-tight">Wipe cache on failed authentication</h3>
          <p class="text-xs text-surface-500 mt-1">
            After the configured number of consecutive failed unlock
            attempts, the encrypted cache is permanently deleted on
            this device. You'll need to re-add your accounts on next
            launch.
          </p>
        </div>
      </div>
      <div class="ml-12 {wipePolicy.enabled ? '' : 'opacity-50 pointer-events-none'}">
        <div class="flex items-center gap-3">
          <label class="text-sm text-surface-700 dark:text-surface-300" for="wipe-max">
            Max attempts
          </label>
          <input
            id="wipe-max"
            type="number"
            min="1"
            placeholder="Unlimited"
            class="input w-32 text-sm px-3 py-1.5 rounded-md"
            bind:value={wipeMaxAttemptsRaw}
            onchange={onWipeMaxAttemptsChange}
            disabled={!wipePolicy.enabled || wipeSaving}
          />
        </div>
        <p class="text-xs text-surface-500 mt-1">
          Empty = unlimited retries (toggle has no effect until a number is set).
        </p>
      </div>
    </div>

    <div
      class="space-y-4 transition-opacity {keyEncryptionEnabled
        ? ''
        : 'opacity-50 pointer-events-none select-none'}"
      aria-disabled={!keyEncryptionEnabled}
    >
      <!-- Hardware-key enrollment is gated on a working WebAuthn
           API.  Linux builds whose libwebkit2gtk lacks WebAuthn
           (the default on most stable distros today) get a
           warning box instead of the form, but passphrase
           enrollment below still works and is the recommended
           path on those systems. -->
      {#if webauthnAvailable}
        <div class="rounded-md border border-surface-200 dark:border-surface-700 p-4">
          <h3 class="font-medium mb-2">Add a hardware key</h3>
          <p class="text-xs text-surface-500 mb-3">
            You'll be asked to authenticate (touch your security key, scan
            a fingerprint, …). The OS handles the prompt; Unkai only sees
            the resulting key material.
          </p>
          <div class="flex items-center gap-2">
            <input
              type="text"
              class="input flex-1 text-sm px-3 py-1.5 rounded-md"
              placeholder="Label — e.g. “YubiKey 5C”, “MacBook Touch ID”"
              bind:value={newLabel}
              disabled={anyBusy}
            />
            <button
              class="btn preset-filled-primary-500"
              disabled={anyBusy}
              onclick={() => void addKey()}
            >{addingKey ? 'Working…' : 'Add'}</button>
          </div>
        </div>
      {:else}
        <div class="rounded-md border border-warning-500/40 bg-warning-500/10 p-4 text-sm text-warning-700 dark:text-warning-300">
          <p class="font-medium mb-1">Hardware-key registration is unavailable on this build.</p>
          <p class="text-xs">
            The webview Tauri uses on Linux (<code>libwebkit2gtk</code>) doesn't
            expose <code>navigator.credentials</code> on most stable distros
            yet.  You can still register a recovery passphrase below — it
            uses the same envelope and unlocks the same way at startup.
            Hardware keys will become available once your distro ships
            WebKitGTK ≥ 2.46 with WebAuthn enabled.
          </p>
        </div>
      {/if}

      {#if !hasPassphrase || passphraseEditing}
        <div class="rounded-md border border-surface-200 dark:border-surface-700 p-4">
          <h3 class="font-medium mb-2">
            {hasPassphrase ? 'Change recovery passphrase' : 'Add a recovery passphrase'}
          </h3>
          <p class="text-xs text-surface-500 mb-3">
            A passphrase derives the same kind of 32-byte key (via
            PBKDF2-HMAC-SHA-256, 720 000 iterations) that a FIDO
            authenticator's PRF output would. Useful as a fallback when a
            hardware key is lost — and as the primary unlock method on
            Linux until WebKitGTK ships the WebAuthn PRF extension.
          </p>
          <div class="space-y-2">
            <input
              type="password"
              class="input w-full text-sm px-3 py-1.5 rounded-md"
              placeholder="Passphrase (8+ characters)"
              bind:value={passphraseValue}
              disabled={anyBusy}
              autocomplete="new-password"
            />
            <input
              type="password"
              class="input w-full text-sm px-3 py-1.5 rounded-md"
              placeholder="Confirm passphrase"
              bind:value={passphraseConfirm}
              disabled={anyBusy}
              autocomplete="new-password"
            />
            <div class="flex gap-2">
              {#if passphraseEditing}
                <button
                  class="btn preset-outlined-surface-500 flex-1"
                  disabled={anyBusy}
                  onclick={() => {
                    passphraseEditing = false
                    passphraseValue = ''
                    passphraseConfirm = ''
                  }}
                >Cancel</button>
              {/if}
              <button
                class="btn preset-filled-primary-500 flex-1"
                disabled={anyBusy || passphraseValue.length < 8}
                onclick={() => void addPassphrase()}
              >{savingPassphrase ? 'Working…' : hasPassphrase ? 'Update passphrase' : 'Save passphrase'}</button>
            </div>
          </div>
        </div>
      {/if}

      <div>
        <h3 class="font-medium mb-2">Registered methods</h3>
        {#if status && status.credentials.length === 0}
          <p class="text-sm text-surface-500 italic">
            No unlock methods registered yet.
          </p>
        {:else if status}
          <ul class="divide-y divide-surface-200 dark:divide-surface-700 rounded-md border border-surface-200 dark:border-surface-700">
            {#each status.credentials as c (c.credentialId)}
              <li class="flex items-center gap-3 p-3">
                <span class="shrink-0 text-surface-600 dark:text-surface-300" aria-hidden="true">
                  <Icon name={c.kind === 'passphrase' ? 'passphrase' : 'security-key'} size={20} />
                </span>
                <div class="flex-1 min-w-0">
                  <!-- For passphrase entries we surface the kind
                       directly; the internal label is an
                       implementation detail the user never set
                       and shouldn't have to see.  Hardware keys
                       keep the user-supplied label since that's
                       what distinguishes one device from another. -->
                  <p class="font-medium truncate">
                    {c.kind === 'passphrase' ? 'Passphrase' : c.label}
                  </p>
                  <p class="text-xs text-surface-500 truncate">
                    {c.kind === 'passphrase' ? 'Recovery method' : 'Hardware key'}
                    · Added {fmtDate(c.createdAt)}
                  </p>
                </div>
                {#if c.kind === 'passphrase'}
                  <button
                    class="btn btn-sm preset-outlined-surface-500"
                    disabled={anyBusy}
                    title="Change passphrase"
                    aria-label="Change passphrase"
                    onclick={() => {
                      passphraseValue = ''
                      passphraseConfirm = ''
                      passphraseEditing = true
                    }}
                  ><Icon name="compose" size={14} /></button>
                {/if}
                <button
                  class="btn btn-sm preset-outlined-error-500"
                  disabled={anyBusy}
                  onclick={() => void removeKey(c)}
                >Remove</button>
              </li>
            {/each}
          </ul>
        {/if}
      </div>

      {#if error}
        <p class="text-sm text-red-500 wrap-break-word">{error}</p>
      {/if}
    </div>
  {/if}

  <!-- PGP passphrase session cache (#341).  Independent of the
       FIDO unlock block above — this controls whether your
       OpenPGP key passphrase is re-prompted on every encrypted
       send/decrypt or remembered for the rest of the running
       app session.  Off by default; the toggle persists in
       localStorage but the cached passphrases themselves live
       only in JS memory and die with the window. -->
  <div class="rounded-md border border-surface-200 dark:border-surface-700 p-4 space-y-3">
    <div class="flex items-start gap-3">
      <Toggle
        checked={isSessionPassphraseEnabled()}
        onchange={(v) => setSessionPassphraseEnabled(v)}
        label={m.security_pgp_remember_toggle_label()}
        class="mt-0.5"
      />
      <div class="flex-1">
        <h3 class="font-medium leading-tight">
          {m.security_pgp_section_title()}
        </h3>
        <p class="text-xs text-surface-500 mt-1 max-w-xl leading-snug">
          {#if !isSessionPassphraseEnabled()}
            {m.security_pgp_remember_toggle_hint_off()}
          {:else if cachedPassphraseCount() === 0}
            {m.security_pgp_remember_toggle_hint_on_empty()}
          {:else if cachedPassphraseCount() === 1}
            {m.security_pgp_remember_toggle_hint_on_cached_one()}
          {:else}
            {m.security_pgp_remember_toggle_hint_on_cached_many({
              count: cachedPassphraseCount(),
            })}
          {/if}
        </p>
      </div>
    </div>
    {#if isSessionPassphraseEnabled()}
      <div class="ml-12">
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500"
          disabled={cachedPassphraseCount() === 0}
          onclick={forgetAllPassphrases}
        >
          {m.security_pgp_forget_button()}
        </button>
      </div>
    {/if}
  </div>
</section>

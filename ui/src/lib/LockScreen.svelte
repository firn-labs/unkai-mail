<script lang="ts">
  /**
   * LockScreen — shown at app startup when the SQLCipher cache
   * is in FIDO-only mode (#164 Phase 1B).  Lists every registered
   * unlock method (hardware keys + the recovery passphrase),
   * runs WebAuthn or a passphrase prompt, and asks Rust to open
   * the cache pool.
   *
   * Once the unlock IPC succeeds the parent (`App.svelte`)
   * receives `onunlock()` and routes the user into the inbox.
   */

  import { tick } from 'svelte'
  import * as api from './api'
  import { formatError } from './errors'
  import { evaluateFidoPrf } from './webauthnPrf'
  import PasswordInput from './PasswordInput.svelte'

  interface Method {
    kind: 'fido_prf' | 'passphrase'
    credentialId: string
    label: string
    salt: string
    createdAt: number
  }

  interface Props {
    methods: Method[]
    /** Remaining unlock attempts before wipe-on-failure fires.
     *  `null` when the policy is off or has no limit set — no
     *  counter is rendered in that case. */
    attemptsRemaining: number | null
    /** Fired after every failed unlock so the parent can refresh
     *  the "X tries remaining" counter without polling. */
    onattemptschange?: (next: number | null) => void
    onunlock: () => void
  }
  let { methods, attemptsRemaining, onattemptschange, onunlock }: Props = $props()

  async function refreshAttempts() {
    try {
      const s = await api.settings.databaseStatus()
      onattemptschange?.(s.attemptsRemaining)
    } catch {
      /* swallow — counter is best-effort */
    }
  }

  let busy = $state(false)
  let error = $state('')
  let passphraseValue = $state('')
  let activeMethod = $state<Method | null>(null)
  let passphraseInput = $state<HTMLInputElement | null>(null)

  // Default focus: prefer a hardware key (one tap, no typing) if
  // any are registered; otherwise show the passphrase prompt.
  $effect(() => {
    if (activeMethod) return
    activeMethod = methods.find((m) => m.kind === 'fido_prf') ?? methods[0] ?? null
  })

  // Focus the passphrase input when it mounts (replaces the
  // `autofocus` attribute, which the linter flags as bad
  // practice — global autofocus can fight a screen reader's
  // own focus management).  The effect runs once the input
  // ref is bound and re-runs if the user switches away to a
  // FIDO method and back.
  $effect(() => {
    if (passphraseInput) passphraseInput.focus()
  })

  async function unlockWithPassphrase() {
    if (busy || !passphraseValue) return
    busy = true
    error = ''
    // Let Svelte flush AND give the WebView a real frame to
    // paint before we hand control off to Rust.  `tick()` +
    // `requestAnimationFrame` alone isn't enough — Tauri's IPC
    // and SQLCipher work on a worker thread, but the WebView
    // still skips repaints if the main thread doesn't yield
    // long enough.  A 32 ms setTimeout (~2 frames) reliably
    // gives the new label a chance to land before unlock
    // starts.
    await tick()
    await new Promise((r) => setTimeout(r, 32))
    try {
      await api.settings.unlockWithPassphrase({ passphrase: passphraseValue })
      passphraseValue = ''
      onunlock()
    } catch (e) {
      error = formatError(e) || 'Unlock failed'
      void refreshAttempts()
    } finally {
      busy = false
    }
  }

  async function unlockWithFido(method: Method) {
    if (busy) return
    busy = true
    error = ''
    await tick()
    await new Promise((r) => setTimeout(r, 32))
    try {
      // OS sheet pops here — Touch ID / Windows Hello / "tap
      // your security key", same UX surface the enrollment
      // already uses.
      const prfOutput = await evaluateFidoPrf(method.credentialId, method.salt)
      await api.settings.unlockWithPrf({
        credentialIdB64: method.credentialId,
        prfOutputB64: prfOutput,
      })
      onunlock()
    } catch (e) {
      error = formatError(e) || 'Unlock failed'
      void refreshAttempts()
    } finally {
      busy = false
    }
  }

  function handleEnter(e: KeyboardEvent) {
    if (e.key === 'Enter' && activeMethod?.kind === 'passphrase') {
      e.preventDefault()
      void unlockWithPassphrase()
    }
  }
</script>

<div class="fixed inset-0 z-[1000] flex items-center justify-center bg-surface-50 dark:bg-surface-900">
  <div class="w-full max-w-md p-8 space-y-6">
    <div class="text-center space-y-1">
      <div class="text-5xl mb-2" aria-hidden="true">🔒</div>
      <h1 class="text-2xl font-semibold">Unkai is locked</h1>
      <p class="text-sm text-surface-500">
        Authenticate to open your encrypted mail cache.
      </p>
      {#if attemptsRemaining != null}
        <p
          class="text-xs font-medium {attemptsRemaining <= 1
            ? 'text-error-500'
            : attemptsRemaining <= 3
              ? 'text-warning-500'
              : 'text-surface-500'}"
        >
          {attemptsRemaining}
          {attemptsRemaining === 1 ? 'try' : 'tries'} remaining before the cache is wiped
        </p>
      {/if}
    </div>

    {#if methods.length === 0}
      <div class="rounded-lg border border-error-500/40 bg-error-500/10 p-4 text-sm">
        No unlock methods are registered.  Use the Settings → Security
        panel to enroll a hardware key or recovery passphrase before
        switching to FIDO-only mode.
      </div>
    {:else}
      <!-- Method picker — shown when more than one method is
           registered.  Single-method case skips straight to the
           form for that method. -->
      {#if methods.length > 1}
        <div class="space-y-2">
          {#each methods as m (m.credentialId)}
            <button
              type="button"
              class="w-full flex items-center gap-3 px-4 py-3 rounded-lg text-left transition-colors
                     border {activeMethod?.credentialId === m.credentialId
                       ? 'border-primary-500 bg-primary-500/10'
                       : 'border-surface-300 dark:border-surface-700 hover:bg-surface-200/60 dark:hover:bg-surface-800/40'}"
              onclick={() => (activeMethod = m)}
              disabled={busy}
            >
              <span class="text-lg" aria-hidden="true">
                {m.kind === 'passphrase' ? '🔐' : '🔑'}
              </span>
              <span class="flex-1 min-w-0">
                <span class="font-medium block truncate">
                  {m.kind === 'passphrase' ? 'Passphrase' : m.label}
                </span>
                <span class="text-xs text-surface-500 block">
                  {m.kind === 'passphrase' ? 'Recovery method' : 'Hardware key / biometric'}
                </span>
              </span>
            </button>
          {/each}
        </div>
      {/if}

      {#if activeMethod?.kind === 'passphrase'}
        <div class="space-y-2">
          <PasswordInput
            inputClass="text-sm px-3 py-2 rounded-lg"
            placeholder="Passphrase"
            bind:value={passphraseValue}
            bind:inputEl={passphraseInput}
            onkeydown={handleEnter}
            disabled={busy}
            autocomplete="current-password"
          />
          <button
            class="btn w-full {busy
              ? 'cursor-wait'
              : !passphraseValue
                ? 'preset-filled-primary-500'
                : 'preset-filled-primary-500'}"
            style={busy ? 'background-color: var(--color-primary-800); color: white;' : ''}
            disabled={!busy && !passphraseValue}
            aria-disabled={busy || !passphraseValue}
            onclick={() => { if (!busy && passphraseValue) void unlockWithPassphrase() }}
          >{busy ? 'Unlocking...' : 'Unlock'}</button>
        </div>
      {:else if activeMethod?.kind === 'fido_prf'}
        <div class="space-y-2">
          <p class="text-sm text-surface-500 text-center">
            Click below — your operating system will ask you to
            authenticate with the registered key.
          </p>
          <button
            class="btn preset-filled-primary-500 w-full {busy ? 'cursor-wait' : ''}"
            style={busy ? '--color-primary-500: var(--color-primary-800); --color-primary-contrast-500: white;' : ''}
            aria-disabled={busy}
            onclick={() => { if (!busy && activeMethod) void unlockWithFido(activeMethod) }}
          >{busy ? 'Awaiting authenticator…' : `Unlock with ${activeMethod.label}`}</button>
        </div>
      {/if}

      {#if error}
        <p class="text-sm text-red-500 wrap-break-word text-center">{error}</p>
      {/if}
    {/if}
  </div>
</div>

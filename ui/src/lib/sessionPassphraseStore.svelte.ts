/**
 * sessionPassphraseStore — opt-in in-memory cache of PGP
 * passphrases for the lifetime of the running app session (#341).
 *
 * Default behaviour matches the #57 v1 design decision: every
 * decrypt or encrypted-send re-prompts for the passphrase, the
 * input is wiped the moment the IPC resolves, no plaintext
 * passphrase outlives the surface that captured it.  This
 * module *opts the user out* of that re-prompting when they
 * flip the Security setting on: the passphrase that
 * successfully unlocked a key is kept in this module's
 * heap-resident Map (keyed by account id) until the toggle is
 * flipped off, the user clicks "Forget cached passphrases",
 * or the app window closes.
 *
 * Why a module store and not a Tauri-side cache?  The user
 * picked the smallest-scope option for this sub-item of #341,
 * and the existing per-message `sessionPassphrase` $state in
 * MailView is the hook this extends.  A backend
 * `RwLock<UnlockedKey>` would survive popout windows for free
 * (separate JS contexts share one Rust process), but it forces
 * a refactor through every PGP Tauri command.  This module
 * stops at the same surface the v1 hook already covered: the
 * main window's Compose / MailView / in-window decrypt prompt.
 * Popped-out windows (StandaloneMail) keep re-prompting — a
 * deliberate v1 limitation worth a follow-up if popout-heavy
 * workflows surface real friction.
 *
 * Storage rules:
 *  - The cache itself never touches disk.  A full app close
 *    wipes the JS heap, which is the strongest guarantee a
 *    pure-frontend cache can offer.
 *  - Only the *enable flag* persists, via localStorage, so the
 *    setting survives restart even though the cache doesn't.
 *  - Flipping the toggle OFF wipes every cached entry, not just
 *    new captures — the user has signalled they want
 *    re-prompting back.
 */

const LOCAL_STORAGE_KEY = 'unkai.rememberSessionPassphrase'

function readEnabledFromStorage(): boolean {
  try {
    return localStorage.getItem(LOCAL_STORAGE_KEY) === '1'
  } catch {
    // Some webview modes don't expose localStorage; treat that
    // as "not enabled" so we fall back to the strict re-prompt
    // behaviour rather than silently caching against the user's
    // expectations.
    return false
  }
}

function writeEnabledToStorage(v: boolean): void {
  try {
    localStorage.setItem(LOCAL_STORAGE_KEY, v ? '1' : '0')
  } catch {
    /* webview may not expose localStorage */
  }
}

// Heap-only `accountId -> passphrase`.  Plain Map: cache reads
// are not reactive (the consumer's IPC payload doesn't need to
// re-run when the cache changes), only the meta-fields below
// are.
const cache = new Map<string, string>()

// Reactive mirror of localStorage so a Settings toggle bound to
// `isEnabled()` updates in step with `setEnabled(...)`.
let enabled = $state(readEnabledFromStorage())

// Reactive size mirror so the Settings UI can render a "N
// accounts currently cached" affordance and grey out the
// Forget button when nothing's cached.  We can't subscribe
// directly to a Map, so every mutator updates this field
// alongside the underlying Map.
let cachedCount = $state(0)

/** Whether the session-passphrase cache is currently opt-in.
 *  Reactive — Svelte components reading this inside an effect
 *  or template re-render when the toggle flips. */
export function isSessionPassphraseEnabled(): boolean {
  return enabled
}

/** Flip the opt-in.  Persists to localStorage so the choice
 *  survives restart.  Turning the toggle *off* always wipes
 *  any currently-cached entries — caching them was conditional
 *  on the user wanting the feature, so consent withdrawal
 *  evicts them immediately. */
export function setSessionPassphraseEnabled(v: boolean): void {
  enabled = v
  writeEnabledToStorage(v)
  if (!v) {
    cache.clear()
    cachedCount = 0
  }
}

/** Stash a passphrase for an account.  No-ops when the user
 *  hasn't opted in, so callers can fire this unconditionally
 *  after every successful decrypt / encrypt without gating
 *  themselves on the setting. */
export function cachePassphrase(accountId: string, passphrase: string): void {
  if (!enabled || !accountId || !passphrase) return
  cache.set(accountId, passphrase)
  cachedCount = cache.size
}

/** Returns the cached passphrase for an account, or `null`.
 *  Also returns `null` whenever caching is disabled — callers
 *  can treat a null result the same way regardless of why. */
export function readCachedPassphrase(accountId: string): string | null {
  if (!enabled || !accountId) return null
  return cache.get(accountId) ?? null
}

/** Drop one account's cached passphrase.  Used when an
 *  IPC rejects the cached value (passphrase changed underneath
 *  us, or key was rotated) so the next attempt re-prompts the
 *  user instead of silently failing in a loop. */
export function forgetPassphrase(accountId: string): void {
  if (cache.delete(accountId)) {
    cachedCount = cache.size
  }
}

/** Nuke everything.  Bound to the Settings "Forget cached
 *  passphrases" button and reused when the user removes their
 *  PGP key entirely. */
export function forgetAllPassphrases(): void {
  if (cache.size === 0) return
  cache.clear()
  cachedCount = 0
}

/** How many distinct accounts currently have a cached
 *  passphrase.  Reactive — drives the Settings UI's "N cached"
 *  copy and Forget-button enabled state. */
export function cachedPassphraseCount(): number {
  return cachedCount
}

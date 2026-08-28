/**
 * accountsStore — single source of truth for the mail-account list
 * (#534 cleanup, modeled on `contactsStore.svelte.ts`).
 *
 * The list used to live twice: as component `$state` in App.svelte
 * *and* re-fetched privately inside AccountSettings, kept in sync
 * through an `onaccountschanged` callback. Lifting it into a
 * `.svelte.ts` module gives every consumer the same reactive array —
 * and chunk 4's profile rail switcher (#535) re-loads exactly this
 * store when a window switches profiles.
 *
 * The store owns the *data*; it does not own selection. Which
 * account is active, and what happens to the selected folder when an
 * account disappears, stays App.svelte's concern.
 */

import * as api from './api'
import type { Account } from './api'

class AccountsStore {
  /** All configured accounts, in backend (insertion) order — the
   *  IconRail and Sidebar sort by `sort_order` themselves. */
  list = $state<Account[]>([])

  /** True until the first load settles; AccountSettings renders its
   *  "Loading accounts..." placeholder off this. */
  loading = $state(true)

  /** Human-readable failure of the last load, '' when it worked. */
  error = $state('')

  /** Re-read the account list from the backend. Returns the fresh
   *  list so callers that need it synchronously (App's selection
   *  logic) don't have to re-read `this.list` after awaiting. */
  async load(): Promise<Account[]> {
    this.loading = this.list.length === 0
    this.error = ''
    try {
      this.list = await api.accounts.getAccounts()
    } catch (e: any) {
      this.error = typeof e === 'string' ? e : (e?.message ?? 'Failed to load accounts')
      throw e
    } finally {
      this.loading = false
    }
    return this.list
  }
}

export const accountsStore = new AccountsStore()

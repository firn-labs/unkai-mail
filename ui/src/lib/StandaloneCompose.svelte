<script lang="ts">
  /**
   * StandaloneCompose — entry component for a popped-out Compose
   * window (#110).  Takes a key from the URL, reads the payload
   * (To/Cc/Bcc/subject/body/attachments/From-account/draftSource/…)
   * stashed in `localStorage` by the launcher, fetches the accounts
   * list, then mounts the regular `Compose` component full-window.
   *
   * Closing semantics: `Compose.onclose` here closes the window,
   * matching the standalone-mail pattern.  Send / save-draft go
   * through the same Tauri commands the modal uses, so the main
   * window's mail-list will pick up the new mail / draft on its
   * next refresh tick.
   */

  import * as api from './api'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import Compose, { type ComposeInitial } from './Compose.svelte'
  import { takeComposePopoutPayload } from './standaloneComposeWindow'
  import { applyTheme, installSystemModeListener } from './theme'

  // Mirror the MailAccount shape Compose.svelte expects.  Defining
  // it locally is simpler than coordinating an export from Compose
  // — the field set is small and stable.
  interface MailAccount {
    id: string
    display_name: string
    email: string
  }

  let { popoutKey }: { popoutKey: string } = $props()

  let accounts = $state<MailAccount[]>([])
  let payload = $state<{ accountId: string; initial: ComposeInitial } | null>(
    null,
  )
  let loadError = $state('')

  $effect(() => {
    let unlistenSystem: (() => void) | null = null
    void (async () => {
      // Theme bootstrap so the popped-out window matches the user's
      // chosen Skeleton theme + light/dark mode.  Same shape as
      // StandaloneMail.svelte (#104).
      try {
        const prefs = await api.settings.getAppSettings()
        applyTheme(prefs.theme_name, prefs.theme_mode)
        unlistenSystem = installSystemModeListener(
          prefs.theme_mode,
          prefs.theme_name,
        )
      } catch (e) {
        console.warn('get_app_settings failed in standalone compose', e)
      }

      // Load the accounts list — Compose's From: picker needs it.
      try {
        accounts = await api.accounts.getAccounts()
      } catch (e) {
        console.warn('get_accounts failed', e)
        loadError = 'Could not load mail accounts.'
        return
      }

      // Read the popout payload from localStorage.  When the key is
      // missing (manual URL fiddling, expired session) fall back to
      // a blank compose against the first configured account so the
      // window isn't an empty shell.
      const stashed = takeComposePopoutPayload(popoutKey)
      if (stashed) {
        payload = stashed
      } else if (accounts.length > 0) {
        payload = { accountId: accounts[0].id, initial: {} }
      } else {
        loadError = 'No popout state found and no accounts configured.'
      }
    })()

    return () => {
      unlistenSystem?.()
    }
  })

  function closeWindow() {
    void getCurrentWindow().close()
  }
</script>

<div class="h-screen w-screen flex flex-col bg-surface-50 dark:bg-surface-900">
  {#if loadError}
    <div class="flex-1 flex items-center justify-center text-sm text-red-500 p-6">
      {loadError}
    </div>
  {:else if payload && accounts.length > 0}
    <!-- Compose normally renders inside a fixed-positioned overlay;
         in the standalone window we let it fill the whole window
         instead.  The component itself doesn't impose a backdrop, so
         a plain flex-fill wrapper does the job.  We keep the
         component's existing close handler — closing the window
         here is the equivalent of dismissing the modal. -->
    <Compose
      accounts={accounts}
      accountId={payload.accountId}
      initial={payload.initial}
      inStandaloneWindow={true}
      onclose={closeWindow}
    />
  {:else}
    <div class="flex-1 flex items-center justify-center text-sm text-surface-500">
      Loading…
    </div>
  {/if}
</div>

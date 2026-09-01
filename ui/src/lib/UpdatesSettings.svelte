<script lang="ts">
  /**
   * Updates settings (#229) — the control surface for the in-app
   * updater.
   *
   *   1. Current-version card with a manual "Check for updates"
   *      button (works even with auto-check off, and ignores a
   *      skipped version — the skip only mutes the rail badge).
   *   2. Update-available card: version, release date + notes from
   *      the manifest, then the two-step download (with progress)
   *      → "Restart now" flow.  Installing restarts the app, so it
   *      only ever happens on that explicit click.
   *   3. Preferences: auto-check, auto-download, release channel.
   *
   * All updater state lives in the shared runes store
   * (`updaterStore.svelte.ts`) — the IconRail badge reads the same
   * object, so this page and the rail can never disagree.
   * Persistence rides the whole-struct `update_app_settings`
   * round-trip (re-fetch → mutate → save, see AiSettings) via the
   * store's `saveUpdaterPrefs`.
   */

  import {
    updater,
    checkForUpdates,
    downloadUpdate,
    installUpdate,
    saveUpdaterPrefs,
    type AppSettingsUpdater,
  } from './updaterStore.svelte'
  import { formatError } from './errors'
  import { notifySettingsChanged } from './settingsBundle'
  import Icon from './Icon.svelte'
  import Toggle from './Toggle.svelte'
  import { m } from '../paraglide/messages'

  interface Props {
    /** Fired after every successful save with the fresh settings
     *  object so AccountSettings/App refresh their cached copies
     *  (same contract as AiSettings' onsettingschanged). */
    onsettingschanged?: (settings: AppSettingsUpdater) => void
  }
  let { onsettingschanged }: Props = $props()

  let saving = $state(false)
  let saveError = $state('')

  /** In-app updates only reach AppImage installs on Linux — a
   *  .deb/.rpm install updates through the distro's package
   *  manager, and the updater plugin refuses to touch it.  The
   *  webview can't see which package format launched it, so the
   *  hint renders on every Linux install. */
  const isLinux = navigator.userAgent.includes('Linux')

  const busy = $derived(updater.downloading || updater.installing)
  const skipped = $derived(
    updater.available?.version != null &&
      updater.available.version === updater.skippedVersion,
  )
  const progressPercent = $derived.by(() => {
    const p = updater.progress
    if (!p || !p.total) return null
    return Math.min(100, Math.round((p.downloaded / p.total) * 100))
  })

  async function save(mutate: (s: AppSettingsUpdater) => void) {
    if (saving) return
    saving = true
    saveError = ''
    try {
      const fresh = await saveUpdaterPrefs(mutate)
      onsettingschanged?.(fresh)
      // The updater prefs travel in the Nextcloud settings bundle
      // (#168) like every other AppSettings field.
      void notifySettingsChanged()
    } catch (e) {
      saveError = formatError(e) || m.settings_updates_error_save()
    } finally {
      saving = false
    }
  }

  function skipThisVersion() {
    const version = updater.available?.version
    if (!version) return
    void save((s) => {
      s.update_skipped_version = version
    })
  }

  function formatDate(unixSeconds: number): string {
    return new Date(unixSeconds * 1000).toLocaleDateString()
  }

  function formatMb(bytes: number): string {
    return (bytes / (1024 * 1024)).toFixed(1)
  }
</script>

<section class="space-y-6">
  <header>
    <h2 class="text-xl font-semibold">{m.settings_updates_title()}</h2>
    <p class="text-sm text-surface-500 mt-1 max-w-xl">
      {m.settings_updates_intro()}
    </p>
  </header>

  <!-- Current version + manual check.  The status line under the
       button is the page's single feedback slot: checking spinner,
       "up to date" success badge, or the manual-check error. -->
  <div class="rounded-lg border border-surface-200 dark:border-surface-700 p-4 space-y-3">
    <div class="flex items-center gap-3">
      <div class="flex-1 min-w-0">
        <h3 class="font-medium leading-tight">{m.settings_updates_current_label()}</h3>
        <p class="text-sm text-surface-500 mt-0.5 font-mono">
          v{updater.currentVersion || '…'}
        </p>
      </div>
      <button
        class="btn btn-sm preset-tonal-surface inline-flex items-center gap-1.5"
        disabled={updater.checking || busy}
        onclick={() => void checkForUpdates({ manual: true })}
      >
        <Icon name={updater.checking ? 'loading' : 'refresh'} size={14} />
        <span>{m.settings_updates_check_button()}</span>
      </button>
    </div>
    {#if updater.checking}
      <p class="text-xs text-surface-500">{m.settings_updates_checking()}</p>
    {:else if updater.error}
      <p class="text-xs text-error-500 wrap-break-word">{updater.error}</p>
    {:else if updater.lastCheckedAt !== null && !updater.available}
      <div class="inline-flex items-center gap-1 text-xs text-success-500" aria-live="polite">
        <Icon name="success" size={14} />
        <span>{m.settings_updates_up_to_date()}</span>
      </div>
    {/if}
  </div>

  <!-- Update available. -->
  {#if updater.available?.version}
    <div class="rounded-lg border border-primary-500/40 bg-primary-500/5 p-4 space-y-3">
      <div class="flex items-center gap-3">
        <div class="flex-1 min-w-0">
          <h3 class="font-medium leading-tight">
            {m.settings_updates_available_title({ version: updater.available.version })}
          </h3>
          {#if updater.available.date}
            <p class="text-xs text-surface-500 mt-0.5">
              {m.settings_updates_available_date({ date: formatDate(updater.available.date) })}
            </p>
          {/if}
        </div>
        {#if updater.downloaded}
          <button
            class="btn btn-sm preset-filled-primary-500 inline-flex items-center gap-1.5"
            disabled={updater.installing}
            onclick={() => void installUpdate()}
          >
            <Icon name={updater.installing ? 'loading' : 'refresh'} size={14} />
            <span>{m.settings_updates_restart_button()}</span>
          </button>
        {:else}
          <button
            class="btn btn-sm preset-filled-primary-500 inline-flex items-center gap-1.5"
            disabled={busy}
            onclick={() => void downloadUpdate()}
          >
            <Icon name={updater.downloading ? 'loading' : 'download'} size={14} />
            <span>{m.settings_updates_download_button()}</span>
          </button>
        {/if}
      </div>

      {#if updater.downloading}
        <div>
          <div class="h-1.5 rounded-full bg-surface-200 dark:bg-surface-700 overflow-hidden">
            {#if progressPercent !== null}
              <div
                class="h-full rounded-full bg-primary-500 transition-[width] duration-300 ease-out"
                style="width: {progressPercent}%"
              ></div>
            {:else}
              <!-- No Content-Length — indeterminate shimmer. -->
              <div class="h-full w-1/3 rounded-full bg-primary-500 animate-pulse"></div>
            {/if}
          </div>
          <p class="text-xs text-surface-500 mt-1">
            {#if updater.progress}
              {formatMb(updater.progress.downloaded)}
              {#if updater.progress.total}/ {formatMb(updater.progress.total)}{/if} MB
            {:else}
              {m.settings_updates_downloading()}
            {/if}
          </p>
        </div>
      {:else if updater.downloaded}
        <div class="inline-flex items-center gap-1 text-xs text-success-500" aria-live="polite">
          <Icon name="success" size={14} />
          <span>{m.settings_updates_downloaded_hint()}</span>
        </div>
      {/if}

      {#if updater.available.notes}
        <div>
          <h4 class="text-xs font-semibold uppercase tracking-wide text-surface-500 mb-1">
            {m.settings_updates_notes_title()}
          </h4>
          <pre
            class="rounded-lg bg-surface-100 dark:bg-surface-800 p-3 text-xs whitespace-pre-wrap wrap-break-word max-h-64 overflow-y-auto">{updater.available.notes}</pre>
        </div>
      {/if}

      {#if skipped}
        <p class="text-xs text-surface-500">
          {m.settings_updates_skipped_note({ version: updater.available.version })}
        </p>
      {:else if !updater.downloaded}
        <button
          class="text-xs text-surface-500 hover:text-surface-700 dark:hover:text-surface-300 underline underline-offset-2"
          disabled={saving}
          onclick={skipThisVersion}
        >
          {m.settings_updates_skip_button()}
        </button>
      {/if}
    </div>
  {/if}

  {#if isLinux}
    <p class="text-xs text-surface-500 max-w-xl">{m.settings_updates_linux_hint()}</p>
  {/if}

  <!-- Preferences. -->
  <div class="rounded-lg border border-surface-200 dark:border-surface-700 p-4 space-y-4">
    <div class="flex items-start gap-3">
      <Toggle
        checked={updater.autoCheck}
        disabled={saving}
        label={m.settings_updates_auto_check_label()}
        onchange={(v) => void save((s) => (s.update_auto_check = v))}
        class="mt-0.5"
      />
      <div class="flex-1">
        <p class="font-medium leading-tight">{m.settings_updates_auto_check_label()}</p>
        <p class="text-xs text-surface-500 leading-snug mt-1 max-w-xl">
          {m.settings_updates_auto_check_hint()}
        </p>
      </div>
    </div>

    <div class="flex items-start gap-3">
      <Toggle
        checked={updater.autoDownload}
        disabled={saving || !updater.autoCheck}
        label={m.settings_updates_auto_download_label()}
        onchange={(v) => void save((s) => (s.update_auto_download = v))}
        class="mt-0.5"
      />
      <div class="flex-1">
        <p class="font-medium leading-tight">{m.settings_updates_auto_download_label()}</p>
        <p class="text-xs text-surface-500 leading-snug mt-1 max-w-xl">
          {m.settings_updates_auto_download_hint()}
        </p>
      </div>
    </div>

    <div>
      <div class="flex items-center gap-3">
        <label class="text-sm text-surface-700 dark:text-surface-300" for="update-channel">
          {m.settings_updates_channel_label()}
        </label>
        <select
          id="update-channel"
          class="select px-2 py-1 text-sm rounded-lg max-w-65"
          value={updater.channel}
          disabled={saving}
          onchange={(e) => {
            const v = (e.currentTarget as HTMLSelectElement).value
            void save((s) => (s.update_channel = v))
          }}
        >
          <option value="stable">{m.settings_updates_channel_stable()}</option>
          <option value="beta">{m.settings_updates_channel_beta()}</option>
        </select>
      </div>
      <p class="text-xs text-surface-500 mt-1 max-w-xl">
        {updater.channel === 'beta'
          ? m.settings_updates_channel_beta_note()
          : m.settings_updates_channel_hint()}
      </p>
    </div>
  </div>

  {#if saveError}
    <p class="text-sm text-error-500 wrap-break-word">{saveError}</p>
  {/if}
</section>

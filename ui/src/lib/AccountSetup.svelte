<script lang="ts">
  /**
   * AccountSetup — a step-by-step wizard for adding a new email account.
   *
   * This is the first thing users see when they launch Unkai for the
   * first time (no accounts configured yet). It collects:
   *   1. Display name + email address
   *   2. IMAP server settings (incoming mail)
   *   3. SMTP server settings (outgoing mail)
   *
   * On submit it calls the `add_account` Tauri command, which persists
   * the account to disk via unkai-store.
   *
   * The component fires an `oncomplete` callback when setup succeeds
   * so the parent (App.svelte) can switch to the inbox view.  When
   * the wizard is invoked from somewhere the user can back out of
   * (e.g. the "Add account" button in Settings, or the IconRail's
   * add-account affordance — both cases where they already have at
   * least one account configured) the parent passes `canCancel=true`
   * and an `oncancel` callback so the wizard can render a close (×)
   * button in the top-right.  On true first launch (zero accounts),
   * `canCancel` defaults to `false` and the button is hidden.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { formatError } from './errors'
  import NextcloudConnect from './NextcloudConnect.svelte'
  import Toggle from './Toggle.svelte'
  import Icon, { type IconName } from './Icon.svelte'
  import RichTextEditor from './RichTextEditor.svelte'
  import { m } from '../paraglide/messages'

  // ── Props ───────────────────────────────────────────────────
  interface Props {
    /** Called when account setup completes successfully. */
    oncomplete: () => void
    /** When true, the wizard renders an "X" close button.  Set by
     *  the parent only when the user has at least one account
     *  configured already — first-launch must finish the wizard. */
    canCancel?: boolean
    /** Called when the user clicks the close button.  Required when
     *  `canCancel` is true. */
    oncancel?: () => void
  }
  let { oncomplete, canCancel = false, oncancel }: Props = $props()

  /**
   * Esc handler for the wizard (#192).  Wired via
   * `<svelte:window onkeydown>` in the template — see the
   * Compose.svelte change for the rationale.  Only fires
   * when `canCancel` is true; first-launch (no accounts) must
   * finish the wizard.
   */
  function onWizardKeydown(e: KeyboardEvent) {
    if (e.key !== 'Escape') return
    if (!canCancel) return
    if (document.querySelector('[role="listbox"]')) return
    e.preventDefault()
    handleCancel()
  }

  // ── Wizard state ────────────────────────────────────────────
  // Which step of the wizard we're on (0-indexed)
  let step = $state(0)
  let error = $state('')
  let saving = $state(false)

  // ── Form fields ─────────────────────────────────────────────
  let displayName = $state('')
  // Sender name (#115) — what appears as the human name in the
  // From: header on outgoing mail.  `displayName` is the local
  // label for the account in the UI; `personName` is the
  // outward-facing identity.  Defaults to `displayName` on the
  // backend when left blank.
  let personName = $state('')
  let email = $state('')
  let password = $state('')     // stored in the OS keychain, never on disk
  let imapHost = $state('')
  let imapPort = $state(993)    // 993 = standard IMAP-over-TLS port
  let smtpHost = $state('')
  let smtpPort = $state(587)    // 587 = standard SMTP submission port
  let useJmap = $state(false)
  // JMAP session base URL. Filled by a provider preset that ships
  // one; left empty otherwise (the backend stores Option<String>).
  let jmapUrl = $state('')
  // Optional plain-text signature appended below new messages from
  // this account. Empty string = no signature; the backend stores it
  // as Option<String>.
  let signature = $state('')

  // ── Provider presets (#413) ─────────────────────────────────
  // A hardcoded pick-list of well-known providers so discovery
  // isn't the only path to working server settings. Mirrors the
  // Rust `ProviderPreset` shape.
  interface ProviderPreset {
    id: string
    display_name: string
    domains: string[]
    imap_host: string
    imap_port: number
    imap_tls: boolean
    smtp_host: string
    smtp_port: number
    smtp_tls: boolean
    jmap_url: string | null
    hint: 'app-password' | 'enable-remote-access' | null
  }
  let presets = $state<ProviderPreset[]>([])
  // 'auto' = no preset picked; run network discovery on email blur.
  let selectedPresetId = $state('auto')
  /** True when the preset was matched from the typed email domain
   *  rather than picked by hand — an auto-match may be revised when
   *  the email changes; an explicit pick is sticky. */
  let presetAutoApplied = $state(false)
  const activePreset = $derived(
    presets.find((p) => p.id === selectedPresetId) ?? null,
  )

  $effect(() => {
    invoke<ProviderPreset[]>('list_provider_presets')
      .then((list) => (presets = list))
      .catch((e) => console.warn('list_provider_presets failed:', e))
  })

  /** Copy a preset's connection settings into the form. Overwrites
   *  the host/port fields on purpose — picking a provider by name is
   *  an explicit "use these settings" request, unlike background
   *  discovery which only fills blanks. */
  function applyPreset(id: string, auto = false) {
    selectedPresetId = id
    presetAutoApplied = auto
    if (id === 'auto') {
      discoveryHint = null
      return
    }
    const p = presets.find((x) => x.id === id)
    if (!p) return
    imapHost = p.imap_host
    imapPort = p.imap_port
    smtpHost = p.smtp_host
    smtpPort = p.smtp_port
    jmapUrl = p.jmap_url ?? ''
    useJmap = !!p.jmap_url
    discoveryHint = m.account_setup_provider_hint_applied({
      provider: p.display_name,
    })
  }

  // ── Step navigation ─────────────────────────────────────────
  // Step metadata drives the numbered progress indicator + the
  // section headers.  Keep `icon` keys in sync with the Icon
  // component's name union.
  // Step metadata: titles are *getters* so they re-resolve when
  // the user switches locale at runtime — paraglide message
  // calls re-run on every access, no cache to invalidate.
  const steps: ReadonlyArray<{ title: () => string; icon: IconName }> = [
    { title: () => m.account_setup_step_your_information(), icon: 'address-book' },
    { title: () => m.account_setup_step_imap(), icon: 'email-envelope' },
    { title: () => m.account_setup_step_smtp(), icon: 'sent' },
    { title: () => m.account_setup_step_nextcloud(), icon: 'cloud' },
  ]
  const totalSteps = steps.length
  /** Index of the step where `submit()` persists the mail account.
   *  Everything after it runs against an already-saved account. */
  const submitStep = 2

  function nextStep() {
    error = ''
    if (step === 0 && (!displayName.trim() || !email.trim())) {
      error = m.account_setup_validation_step0()
      return
    }
    if (step === 1 && (!imapHost.trim() || !password)) {
      error = m.account_setup_validation_step1()
      return
    }
    if (step === 2 && !smtpHost.trim()) {
      error = m.account_setup_validation_step2()
      return
    }
    step++
  }

  function prevStep() {
    error = ''
    step--
  }

  function handleCancel() {
    if (!canCancel) return
    // Once the mail account is saved (i.e. the user is on the
    // optional Nextcloud step), closing the wizard must land in the
    // normal "account exists" flow, not the pre-setup cancel path —
    // the parent needs to reload the account list either way.
    if (accountCreated) {
      oncomplete()
      return
    }
    oncancel?.()
  }

  // ── Optional Nextcloud step (#413) ──────────────────────────
  // The mail account is persisted at the end of the SMTP step; the
  // last step offers the Nextcloud Login Flow v2 connect that used
  // to live only in Settings. Skipping is always possible.
  interface NcCapabilitiesLite {
    caldav: boolean
    carddav: boolean
    tasks?: boolean
  }
  interface NcAccountLite {
    id: string
    server_url: string
    username: string
    display_name?: string | null
    capabilities?: NcCapabilitiesLite | null
  }
  /** True once `add_account` succeeded — the wizard's remaining
   *  steps are optional extras on top of a saved account. */
  let accountCreated = $state(false)
  let ncAccount = $state<NcAccountLite | null>(null)

  function onNcConnected(acct: NcAccountLite) {
    ncAccount = acct
    // First-time sync (#318): a freshly-connected NC has no local
    // contacts/calendars yet — kick the initial pulls off in the
    // background so the integration views aren't empty when the
    // user lands in the app. Fire-and-forget: failures show up in
    // the Settings sync rows, not here.
    void seedInitialSync(acct)
  }

  async function seedInitialSync(acct: NcAccountLite) {
    const caps = acct.capabilities
    if (!caps) return
    const jobs: Promise<unknown>[] = []
    if (caps.carddav) {
      jobs.push(invoke('sync_nextcloud_contacts', { ncId: acct.id }))
    }
    if (caps.caldav) {
      jobs.push(invoke('sync_nextcloud_calendars', { ncId: acct.id }))
    }
    await Promise.allSettled(jobs)
    // Task lists piggy-back on CalDAV but need their own discovery
    // + per-list sync round-trip (#92).
    if (caps.tasks && caps.caldav) {
      try {
        const lists = await invoke<{ id: string }[]>(
          'sync_nextcloud_task_lists',
          { ncId: acct.id },
        )
        await Promise.allSettled(
          lists.map((l) =>
            invoke('sync_nextcloud_tasks', { ncId: acct.id, listId: l.id }),
          ),
        )
      } catch (e) {
        console.warn('initial task-list sync failed:', e)
      }
    }
  }

  // ── Auto-fill server settings from email domain ─────────────
  // When the user blurs the email field we ask the backend to
  // probe Mozilla autoconfig and DNS SRV for that domain. If
  // anything comes back we prefill the IMAP/SMTP fields with the
  // discovered hosts/ports — the user can still edit them on the
  // next step. If nothing comes back we fall back to the naive
  // `imap.<domain>` / `smtp.<domain>` heuristic so the form
  // doesn't look completely empty.
  let discovering = $state(false)
  let discoveryHint = $state<string | null>(null)

  interface DiscoveredAccount {
    imap_host: string
    imap_port: number
    imap_tls: boolean
    smtp_host: string
    smtp_port: number
    smtp_tls: boolean
    source: 'autoconfig-domain' | 'autoconfig-ispdb' | 'srv' | 'preset'
  }

  async function autoFillServers() {
    if (!email.includes('@')) return
    const domain = email.split('@')[1]
    // A hand-picked preset wins over anything the email domain says —
    // the user explicitly chose it. Auto-matched presets get
    // re-evaluated so changing the address to another provider's
    // domain doesn't leave stale settings behind.
    if (selectedPresetId !== 'auto' && !presetAutoApplied) return
    const matched = presets.find((p) =>
      p.domains.includes(domain.trim().toLowerCase()),
    )
    if (matched) {
      applyPreset(matched.id, true)
      return
    }
    if (presetAutoApplied) {
      // Previous auto-match no longer applies — fall back to
      // discovery below (it only fills blank fields, so nothing
      // the user typed is lost).
      selectedPresetId = 'auto'
      presetAutoApplied = false
    }
    discoveryHint = null
    discovering = true
    try {
      const found = await invoke<DiscoveredAccount | null>(
        'discover_account_settings',
        { email: email.trim() },
      )
      if (found) {
        // Only overwrite blank fields so a user mid-edit doesn't
        // lose what they typed. Same posture as the old heuristic.
        if (!imapHost) imapHost = found.imap_host
        imapPort = found.imap_port
        if (!smtpHost) smtpHost = found.smtp_host
        smtpPort = found.smtp_port
        const label =
          found.source === 'autoconfig-domain'
            ? m.account_setup_discovery_label_provider()
            : found.source === 'autoconfig-ispdb'
              ? m.account_setup_discovery_label_ispdb()
              : found.source === 'preset'
                ? m.account_setup_discovery_label_preset()
                : m.account_setup_discovery_label_srv()
        discoveryHint = m.account_setup_discovery_hint_found({ source: label })
        return
      }
    } catch (e) {
      console.warn('discover_account_settings failed:', e)
    } finally {
      discovering = false
    }

    // Fallback heuristic when discovery returns nothing.
    if (!imapHost) imapHost = `imap.${domain}`
    if (!smtpHost) smtpHost = `smtp.${domain}`
    discoveryHint = m.account_setup_discovery_hint_fallback({ domain })
  }

  // ── TLS-trust prompt state ─────────────────────────────────
  // When test_connection fails because the IMAP server's cert
  // can't be validated, we show a prompt that lets the user trust
  // the chain and retry. The flow:
  //   1. submit() catches the cert error from test_connection
  //   2. invoke probe_server_certificate to capture the full chain
  //   3. show fingerprints + "Trust this server" button
  //   4. user confirms → every cert in the chain gets added to
  //      trustedCerts → retry submit
  // The list rides through to add_account so the saved account
  // remembers the trust decision and uses it on future connects.
  // Trusting the whole chain (not just the leaf) means a leaf
  // reissue under the same intermediate, or a server that
  // reorders its chain, doesn't drop the user back into this
  // prompt.
  interface ProbedCertEntry {
    der: number[]
    sha256: string
  }
  interface ProbedCert {
    chain: ProbedCertEntry[]
    host: string
  }
  /** Full Rust `TrustedCert` shape — what `test_connection` and
      `add_account` both deserialize. The `added_at` epoch is
      stamped at trust time so we don't depend on the user
      finishing the wizard before the timestamp is set. */
  interface TrustedCert {
    der: number[]
    sha256: string
    host: string
    added_at: number
  }
  let pendingCert = $state<ProbedCert | null>(null)
  let trustedCerts = $state<TrustedCert[]>([])

  /** Heuristic: does this error message look like it came from a
      TLS cert validation failure? rustls's wording is fairly stable
      ("invalid peer certificate", "UnknownIssuer", etc.) but we
      cast a wide net to be tolerant of OS-level wrappers. */
  function looksLikeCertError(message: string): boolean {
    const m = message.toLowerCase()
    return (
      m.includes('certificate') ||
      m.includes('cert ') ||
      m.includes('unknownissuer') ||
      m.includes('untrustedissuer') ||
      m.includes('badcertificate') ||
      m.includes('tls handshake')
    )
  }

  async function handleCertError() {
    pendingCert = null
    try {
      const probed = await invoke<ProbedCert>('probe_server_certificate', {
        host: imapHost.trim(),
        port: imapPort,
      })
      pendingCert = probed
    } catch (e: any) {
      error = m.account_setup_cert_probe_failed({
        reason: formatError(e) || 'unknown error',
      })
    }
  }

  function trustPendingCert() {
    if (!pendingCert) return
    // Promote every cert in the probed chain → `TrustedCert`. We
    // trust the whole chain, not just the leaf, so a server that
    // reorders the chain on a future connect (or reissues the leaf
    // under the same intermediate) still validates without
    // re-prompting the user.
    const addedAt = Math.floor(Date.now() / 1000)
    const host = pendingCert.host
    const additions: TrustedCert[] = pendingCert.chain.map((entry) => ({
      der: entry.der,
      sha256: entry.sha256,
      host,
      added_at: addedAt,
    }))
    trustedCerts = [...trustedCerts, ...additions]
    pendingCert = null
    void submit()
  }

  function dismissCertPrompt() {
    pendingCert = null
  }

  // ── Submit ──────────────────────────────────────────────────
  async function submit() {
    error = ''
    saving = true

    try {
      // Probe the IMAP server with the entered credentials *before*
      // persisting anything. This turns "saved a bad account and
      // everything breaks silently on first fetch" into a clear,
      // immediate error the user can act on (wrong host, wrong port,
      // TLS failure, bad password — all surface here). `trustedCerts`
      // grows when the user accepts a self-signed cert via the
      // prompt below, so the same probe will pass on the retry.
      await invoke('test_connection', {
        host: imapHost.trim(),
        port: imapPort,
        username: email.trim(),
        password,
        trustedCerts: trustedCerts.length > 0 ? trustedCerts : null,
      })

      // Generate a simple unique ID for this account.
      // crypto.randomUUID() is available in all modern browsers
      // (and Tauri's webview).
      const id = crypto.randomUUID()

      // Call the Rust backend to save this account. The password is
      // handed over as a separate argument so the Rust side can stash
      // it in the OS keychain — it never gets written to accounts.json.
      await invoke('add_account', {
        account: {
          id,
          display_name: displayName.trim(),
          person_name: personName.trim() || null,
          email: email.trim(),
          imap_host: imapHost.trim(),
          imap_port: imapPort,
          smtp_host: smtpHost.trim(),
          smtp_port: smtpPort,
          use_jmap: useJmap,
          jmap_url: jmapUrl.trim() || null,
          signature: signature.trim() || null,
          folder_icons: [],
          // `trustedCerts` already carries `added_at` from when the
          // user accepted each cert, so it ships through unchanged.
          trusted_certs: trustedCerts,
        },
        password,
      })

      // Success! The mail account exists now — move on to the
      // optional Nextcloud step instead of closing the wizard.
      // `oncomplete` fires when the user finishes or skips it.
      accountCreated = true
      error = ''
      step = submitStep + 1
    } catch (e: any) {
      const msg = formatError(e) || m.account_setup_save_failed()
      if (looksLikeCertError(msg)) {
        // Don't surface the raw error — the prompt explains the
        // situation more clearly. Kick off the cert probe in the
        // background; UI shows a spinner until it returns.
        error = ''
        void handleCertError()
      } else {
        error = msg
      }
    } finally {
      saving = false
    }
  }
</script>

<!--
  The wizard is a centered card.  Layout from top to bottom:
    * brand title + tagline
    * the card, with:
        - top-right "×" button (only when `canCancel`)
        - numbered step indicator (1 ── 2 ── 3, completed steps
          show a check mark, the active one is filled-primary)
        - icon-prefixed section header for the current step
        - the form fields for the current step
        - error / cert-trust prompts
        - Back / Next | Add Account buttons
-->
<svelte:window onkeydown={onWizardKeydown} />
<!-- #317 — outer wrapper scrolls when content exceeds the viewport
     (e.g. the rich-text signature editor on step 3 pushes the Add
     Account button below the fold on shorter windows).  Previously
     `h-full flex items-center` centred the card vertically but
     clipped overflow on both sides with no scroll.  The fix is a
     two-layer pattern: the outer takes `h-full overflow-y-auto`,
     the middle takes `min-h-full flex items-center` so the card
     stays visually centred when it fits and the page just grows
     and scrolls when it doesn't.  `py-8` gives breathing room
     against the viewport edges during scroll. -->
<div class="h-full overflow-y-auto bg-surface-50 dark:bg-surface-900">
  <div class="min-h-full flex items-center justify-center py-8">
    <div class="w-full max-w-xl mx-4">
    <!-- Header -->
    <div class="text-center mb-8">
      <h1 class="text-3xl font-bold text-primary-500 mb-2">{m.account_setup_welcome_title()}</h1>
      <p class="text-surface-600 dark:text-surface-400">{m.account_setup_welcome_subtitle()}</p>
    </div>

    <!-- Card.  When the wizard is closeable, we add extra top
         padding so the corner-anchored "×" button doesn't crowd
         the step indicator below it. -->
    <div
      class="card relative p-6 {canCancel ? 'pt-10' : ''} bg-surface-100 dark:bg-surface-800 rounded-xl shadow-lg"
    >
      {#if canCancel}
        <button
          type="button"
          class="absolute top-1 right-1 p-1.5 rounded-md text-surface-500 hover:text-surface-900 hover:bg-surface-200 dark:hover:text-surface-100 dark:hover:bg-surface-700 transition-colors"
          onclick={handleCancel}
          aria-label={m.account_setup_close_label()}
          title={m.account_setup_close_title()}
        >
          <Icon name="close" size={18} />
        </button>
      {/if}

      <!-- Numbered step indicator.  Each step is a circle (active /
           completed / pending) connected by a thin line.  The active
           step's circle is filled-primary, completed steps show the
           check icon, pending steps show the step number. -->
      <div class="flex items-center justify-center mb-6 px-2">
        {#each steps as s, i (s.icon)}
          {#if i > 0}
            <div
              class="flex-1 h-px mx-2 transition-colors {i <= step
                ? 'bg-primary-500'
                : 'bg-surface-300 dark:bg-surface-600'}"
            ></div>
          {/if}
          <div class="flex flex-col items-center gap-1">
            <div
              class="w-8 h-8 rounded-full flex items-center justify-center text-xs font-semibold transition-colors {i ===
              step
                ? 'bg-primary-500 text-white'
                : i < step
                  ? 'bg-primary-500/20 text-primary-700 dark:text-primary-300'
                  : 'bg-surface-200 dark:bg-surface-700 text-surface-500'}"
            >
              {#if i < step}
                <Icon name="success" size={14} />
              {:else}
                {i + 1}
              {/if}
            </div>
            <span
              class="text-[10px] uppercase tracking-wide font-medium {i === step
                ? 'text-primary-600 dark:text-primary-400'
                : 'text-surface-500'}"
            >
              {m.account_setup_step_label({ n: i + 1 })}
            </span>
          </div>
        {/each}
      </div>

      <!-- Section header for the current step (icon + title). -->
      <div class="flex items-center gap-2 mb-4">
        <span class="text-primary-500"><Icon name={steps[step].icon} size={20} /></span>
        <h2 class="text-lg font-semibold">{steps[step].title()}</h2>
      </div>

      <!-- Step 0: Basic info -->
      {#if step === 0}
        <div>
          <label class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_account_name_label()}</span>
            <div class="relative mt-1">
              <span class="absolute left-2.5 top-1/2 -translate-y-1/2 text-surface-400 pointer-events-none flex items-center" aria-hidden="true">
                <Icon name="design-palette" size={14} />
              </span>
              <input
                type="text"
                bind:value={displayName}
                placeholder={m.account_setup_account_name_placeholder()}
                class="input w-full pl-8 pr-3 py-2 rounded-md"
              />
            </div>
            <span class="block text-xs text-surface-500 mt-1">
              {m.account_setup_account_name_hint()}
            </span>
          </label>
          <label class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_your_name_label()}</span>
            <div class="relative mt-1">
              <span class="absolute left-2.5 top-1/2 -translate-y-1/2 text-surface-400 pointer-events-none flex items-center" aria-hidden="true">
                <Icon name="contacts" size={14} />
              </span>
              <input
                type="text"
                bind:value={personName}
                placeholder={m.account_setup_your_name_placeholder()}
                class="input w-full pl-8 pr-3 py-2 rounded-md"
              />
            </div>
            <span class="block text-xs text-surface-500 mt-1">
              {m.account_setup_your_name_hint()}
            </span>
          </label>
          <label class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_email_label()}</span>
            <div class="relative mt-1">
              <span class="absolute left-2.5 top-1/2 -translate-y-1/2 text-surface-400 pointer-events-none flex items-center" aria-hidden="true">
                <Icon name="email-envelope" size={14} />
              </span>
              <input
                type="email"
                bind:value={email}
                placeholder={m.account_setup_email_placeholder()}
                class="input w-full pl-8 pr-3 py-2 rounded-md"
                onblur={autoFillServers}
                disabled={discovering}
              />
            </div>
            {#if discovering}
              <span class="text-xs text-surface-500 mt-1 flex items-center gap-1">
                <Icon name="loading" size={12} />
                {m.account_setup_email_discovering()}
              </span>
            {:else if discoveryHint}
              <span class="text-xs text-surface-500 mt-1 flex items-center gap-1">
                <Icon name="info" size={12} />
                {discoveryHint}
              </span>
            {/if}
          </label>

          <!-- Provider pick-list (#413).  Picking a known provider
               copies its published server settings into the IMAP/SMTP
               steps — discovery is no longer the only path.  The
               fields on the next steps stay editable, so manual
               override always works. -->
          <label class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_provider_label()}</span>
            <select
              class="select w-full mt-1 px-3 py-2 text-sm rounded-md"
              bind:value={selectedPresetId}
              onchange={() => applyPreset(selectedPresetId)}
            >
              <option value="auto">{m.account_setup_provider_auto()}</option>
              {#each presets as p (p.id)}
                <option value={p.id}>{p.display_name}</option>
              {/each}
            </select>
            <span class="block text-xs text-surface-500 mt-1">
              {m.account_setup_provider_hint()}
            </span>
          </label>
        </div>

      <!-- Step 1: IMAP settings -->
      {:else if step === 1}
        <div>
          <p class="text-sm text-surface-500 mb-4">
            {m.account_setup_imap_explainer()}
          </p>
          <!-- Preset-specific requirement (#413): some providers ship
               with remote access off, or reject the normal account
               password over IMAP.  Surface that *before* the user
               types credentials that can't work. -->
          {#if activePreset?.hint === 'enable-remote-access'}
            <div class="text-xs text-surface-600 dark:text-surface-400 mb-4 p-3 rounded-md border border-warning-500/40 bg-warning-500/5 flex items-start gap-2">
              <span class="mt-0.5"><Icon name="warning" size={14} /></span>
              <span>{m.account_setup_provider_hint_enable_remote_access({ provider: activePreset.display_name })}</span>
            </div>
          {/if}
          <label class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_imap_server_label()}</span>
            <div class="relative mt-1">
              <span class="absolute left-2.5 top-1/2 -translate-y-1/2 text-surface-400 pointer-events-none flex items-center" aria-hidden="true">
                <Icon name="cloud" size={14} />
              </span>
              <input
                type="text"
                bind:value={imapHost}
                placeholder={m.account_setup_imap_server_placeholder()}
                class="input w-full pl-8 pr-3 py-2 rounded-md"
              />
            </div>
          </label>
          <label class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_port_label()}</span>
            <input
              type="number"
              bind:value={imapPort}
              class="input w-full mt-1 px-3 py-2 rounded-md"
            />
          </label>
          <label class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_password_label()}</span>
            <div class="relative mt-1">
              <span class="absolute left-2.5 top-1/2 -translate-y-1/2 text-surface-400 pointer-events-none flex items-center" aria-hidden="true">
                <Icon name="lock" size={14} />
              </span>
              <input
                type="password"
                bind:value={password}
                placeholder={m.account_setup_password_placeholder()}
                class="input w-full pl-8 pr-3 py-2 rounded-md"
                autocomplete="current-password"
              />
            </div>
            <span class="block text-xs text-surface-500 mt-1">
              {m.account_setup_password_hint()}
            </span>
            {#if activePreset?.hint === 'app-password'}
              <span class="block text-xs text-warning-600 dark:text-warning-400 mt-1">
                {m.account_setup_provider_hint_app_password({ provider: activePreset.display_name })}
              </span>
            {/if}
          </label>
        </div>

      <!-- Step 2: SMTP settings -->
      {:else if step === 2}
        <div>
          <p class="text-sm text-surface-500 mb-4">
            {m.account_setup_smtp_explainer()}
          </p>
          <label class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_smtp_server_label()}</span>
            <div class="relative mt-1">
              <span class="absolute left-2.5 top-1/2 -translate-y-1/2 text-surface-400 pointer-events-none flex items-center" aria-hidden="true">
                <Icon name="cloud" size={14} />
              </span>
              <input
                type="text"
                bind:value={smtpHost}
                placeholder={m.account_setup_smtp_server_placeholder()}
                class="input w-full pl-8 pr-3 py-2 rounded-md"
              />
            </div>
          </label>
          <label class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_port_label()}</span>
            <input
              type="number"
              bind:value={smtpPort}
              class="input w-full mt-1 px-3 py-2 rounded-md"
            />
          </label>

          <!-- JMAP toggle.  A modern protocol some servers offer
               in addition to (or instead of) IMAP/SMTP. -->
          <div class="flex items-center justify-between gap-3 mb-4 p-3 rounded-md bg-surface-200/50 dark:bg-surface-700/40">
            <div class="flex items-start gap-2 min-w-0">
              <span class="text-primary-500 mt-0.5"><Icon name="sync" size={16} /></span>
              <div class="min-w-0">
                <span class="block text-sm font-medium text-surface-700 dark:text-surface-200">{m.account_setup_jmap_label()}</span>
                <span class="block text-xs text-surface-500">
                  {m.account_setup_jmap_hint()}
                </span>
              </div>
            </div>
            <Toggle bind:checked={useJmap} label={m.account_setup_jmap_label()} />
          </div>

          <!-- #317 — must be a plain <div>, not a <label>.  A <label>
               wrapping the RichTextEditor redirects click focus to the
               first labelable form control inside (one of the toolbar
               <button>s), so clicking into the contenteditable would
               immediately un-focus the editor and leave the user
               unable to type.  The text below uses a <span> rather
               than a <label for=…> because Tiptap's contenteditable
               isn't a labelable form control. -->
          <div class="block mb-4">
            <span class="text-sm font-medium text-surface-700 dark:text-surface-300">{m.account_setup_signature_label()}</span>
            <!-- #248 — rich-text signature editor.  HTML output rides
                 through unchanged via `Account.signature`; Compose's
                 `signatureBlock` detects the HTML shape and passes it
                 through verbatim (legacy plain-text signatures still
                 work via the same code path). -->
            <div class="signature-editor-shell mt-1">
              <RichTextEditor
                content={signature}
                placeholder={`Jane Doe\nProduct Manager · Example Corp\n+1 555 0100`}
                onchange={(html) => (signature = html)}
              />
            </div>
            <span class="block text-xs text-surface-500 mt-1">
              {m.account_setup_signature_hint()}
            </span>
          </div>
        </div>

      <!-- Step 3: optional Nextcloud connect (#413).  Reached only
           after `add_account` succeeded — the shared NextcloudConnect
           card drives the Login Flow v2; the backend persists the
           account itself. -->
      {:else if step === 3}
        <div>
          <p class="text-sm text-surface-500 mb-4">
            {m.account_setup_nextcloud_explainer()}
          </p>
          {#if ncAccount}
            <div class="mb-4 p-3 rounded-md border border-success-500/30 bg-success-500/5 text-sm text-surface-700 dark:text-surface-300 flex items-start gap-2">
              <span class="text-success-500 mt-0.5"><Icon name="success" size={16} /></span>
              <span>
                {m.account_setup_nextcloud_connected({
                  user: ncAccount.display_name ?? ncAccount.username,
                  server: ncAccount.server_url,
                })}
              </span>
            </div>
          {:else}
            <NextcloudConnect onconnected={onNcConnected} />
          {/if}
        </div>
      {/if}

      <!-- Error message -->
      {#if error}
        <div class="text-sm text-red-500 mb-4 p-3 bg-red-500/10 rounded-md flex items-start gap-2">
          <span class="mt-0.5"><Icon name="error" size={16} /></span>
          <span>{error}</span>
        </div>
      {/if}

      <!-- TLS-trust prompt. Shown when test_connection failed with a
           cert error and probe_server_certificate succeeded in
           capturing the leaf cert. The user gets the SHA-256 to
           compare against their server, then chooses whether to
           trust it for this account. -->
      {#if pendingCert}
        <div class="mb-4 p-4 rounded-md border border-warning-500/40 bg-warning-500/5">
          <p class="text-sm font-medium mb-1 flex items-center gap-2">
            <Icon name="lock" size={16} />
            {m.account_setup_cert_title()}
          </p>
          <p class="text-xs text-surface-500 mb-3">
            {m.account_setup_cert_explainer()}
          </p>
          <p class="text-xs mb-1"><span class="text-surface-500">{m.account_setup_cert_host_label()}</span> <span class="font-mono">{pendingCert.host}</span></p>
          <div class="text-xs mb-3">
            <p class="text-surface-500 mb-1">
              {#if pendingCert.chain.length === 1}
                {m.account_setup_cert_fingerprint_one()}
              {:else if pendingCert.chain.length === 2}
                {m.account_setup_cert_fingerprint_many({ n: 1 })}
              {:else}
                {m.account_setup_cert_fingerprint_many_plural({ n: pendingCert.chain.length - 1 })}
              {/if}
            </p>
            <ul class="space-y-1">
              {#each pendingCert.chain as entry, i (entry.sha256)}
                <li class="font-mono break-all">
                  <span class="text-surface-500">{i === 0 ? 'leaf:' : `int${i}:`}</span>
                  {entry.sha256}
                </li>
              {/each}
            </ul>
          </div>
          <div class="flex gap-2">
            <button
              type="button"
              class="btn btn-sm preset-filled-primary-500"
              onclick={trustPendingCert}
            >{m.account_setup_cert_button_trust()}</button>
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500"
              onclick={dismissCertPrompt}
            >{m.account_setup_cert_button_cancel()}</button>
          </div>
        </div>
      {/if}

      {#if trustedCerts.length > 0 && !pendingCert}
        <div class="mb-4 p-3 rounded-md border border-success-500/30 bg-success-500/5 text-xs text-surface-600 dark:text-surface-400 flex items-center gap-2">
          <Icon name="verified" size={14} />
          {#if trustedCerts.length === 1}
            {m.account_setup_cert_trusting_one()}
          {:else}
            {m.account_setup_cert_trusting_many({ n: trustedCerts.length })}
          {/if}
        </div>
      {/if}

      <!-- Navigation buttons.  Back is hidden on the Nextcloud step:
           the mail account is already saved at that point, so going
           "back" into the credential steps would suggest edits that
           won't be re-submitted. -->
      <div class="flex justify-between mt-6">
        {#if step > 0 && step <= submitStep}
          <button class="btn preset-outlined-surface-500 flex items-center gap-1" onclick={prevStep}>
            <Icon name="arrow-left" size={14} />
            {m.account_setup_button_back()}
          </button>
        {:else}
          <div></div>
        {/if}

        {#if step < submitStep}
          <button class="btn preset-filled-primary-500 flex items-center gap-1" onclick={nextStep}>
            {m.account_setup_button_next()}
            <Icon name="arrow-right" size={14} />
          </button>
        {:else if step === submitStep}
          <button
            class="btn preset-filled-primary-500 flex items-center gap-1"
            onclick={submit}
            disabled={saving}
          >
            {#if saving}
              <Icon name="loading" size={14} />
              {m.account_setup_button_saving()}
            {:else}
              <Icon name="add-account" size={14} />
              {m.account_setup_button_add_account()}
            {/if}
          </button>
        {:else}
          <button
            class="btn preset-filled-primary-500 flex items-center gap-1"
            onclick={() => oncomplete()}
          >
            {#if ncAccount}
              <Icon name="success" size={14} />
              {m.account_setup_button_finish()}
            {:else}
              {m.account_setup_button_skip()}
              <Icon name="arrow-right" size={14} />
            {/if}
          </button>
        {/if}
      </div>
    </div>
    </div>
  </div>
</div>

<style>
  /* #248 — fixed-height shell so the embedded `RichTextEditor`
     has bounded vertical room (its scroller is `flex-1 min-h-0`).
     Same shape as the signature shell in AccountSettings. */
  :global(.signature-editor-shell) {
    height: 16rem;
    min-height: 16rem;
    border: 1px solid var(--color-surface-300);
    border-radius: 0.375rem;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  :global([data-mode='dark'] .signature-editor-shell) {
    border-color: var(--color-surface-700);
  }
</style>

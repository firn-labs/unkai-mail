<script lang="ts">
  /**
   * NextcloudConnect — the reusable "connect a Nextcloud server"
   * card (#413).
   *
   * Extracted from NextcloudSettings so the account-setup wizard can
   * offer the same Login Flow v2 connect step without duplicating the
   * polling / TLS-trust logic. Both surfaces render this component;
   * the flow is:
   *
   * 1. User types their NC server URL and clicks "Connect".
   * 2. We call `start_nextcloud_login` to get a browser URL + poll
   *    handle, then open the URL via `open_url`.
   * 3. We poll `poll_nextcloud_login` every 2s until the server
   *    returns the app password (user approved in the browser) or
   *    the user cancels.
   * 4. The backend persists the account + keychain secret itself;
   *    we hand the resulting account record to `onconnected`.
   *
   * Self-signed certificates (#253): a TLS failure during step 2
   * triggers `probe_server_certificate`, the user reviews the chain
   * fingerprints, and on confirm we retry with the chain in the
   * trust list. The trust list rides through the polling call so the
   * saved account record remembers the decision.
   */

  import * as api from './api'
  import { formatError } from './errors'
  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'

  // ── Types (mirror the Rust models) ──────────────────────────
  // Consumers keep their own structural copies of these shapes —
  // TypeScript matches them structurally, so there's no need to
  // export from an instance script (which runes mode disallows).
  interface NextcloudCapabilities {
    version?: string | null
    talk: boolean
    files: boolean
    caldav: boolean
    carddav: boolean
    office?: boolean
    notes?: boolean
    tasks?: boolean
  }
  interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
    capabilities?: NextcloudCapabilities | null
    trusted_certs?: TrustedCert[]
  }
  /** Mirror of the Rust `TrustedCert` shape. */
  interface TrustedCert {
    der: number[]
    sha256: string
    host: string
    added_at: number
  }
  interface ProbedCertEntry {
    der: number[]
    sha256: string
  }
  interface ProbedCert {
    chain: ProbedCertEntry[]
    host: string
  }
  interface LoginFlowInit {
    login_url: string
    poll_token: string
    poll_endpoint: string
  }

  // ── Props ───────────────────────────────────────────────────
  interface Props {
    /** Fired once the login flow completes and the backend has
     *  persisted the new account. */
    onconnected: (account: NextcloudAccount) => void
  }
  let { onconnected }: Props = $props()

  // ── State ───────────────────────────────────────────────────
  let serverInput = $state('')
  let error = $state('')
  let connecting = $state(false)      // true while a login is in flight
  let pollTimer: number | null = null // setInterval handle, so we can cancel
  // Shown so the user can click/copy the URL if auto-open didn't work.
  let pendingLoginUrl = $state('')

  // Self-signed cert support (#253) — same shape as AccountSetup's
  // IMAP-side prompt.
  let trustedCerts = $state<TrustedCert[]>([])
  let pendingCert = $state<ProbedCert | null>(null)

  // Cleanup: cancel any in-flight polling if the component unmounts.
  $effect(() => () => stopPolling())

  /** Heuristic: does `msg` look like a TLS-trust failure?  Same
   *  fingerprint-style match `AccountSetup.svelte` uses for IMAP. */
  function looksLikeCertError(msg: string): boolean {
    const lowered = msg.toLowerCase()
    return (
      lowered.includes('certificate') ||
      lowered.includes('cert ') ||
      lowered.includes('unknownissuer') ||
      lowered.includes('untrustedissuer') ||
      lowered.includes('badcertificate') ||
      lowered.includes('tls handshake') ||
      lowered.includes('invalid peer certificate')
    )
  }

  /** Pull host (and explicit port if any) out of a server URL so
   *  we can hand it to `probe_server_certificate`.  Defaults to
   *  443 — the only port Nextcloud realistically serves on. */
  function hostPortFromUrl(url: string): { host: string; port: number } | null {
    try {
      const u = new URL(url)
      return {
        host: u.hostname,
        port: u.port ? Number.parseInt(u.port, 10) : 443,
      }
    } catch {
      return null
    }
  }

  async function handleNcCertError() {
    pendingCert = null
    const url = serverInput.trim()
    const normalised = /^https?:\/\//.test(url) ? url : `https://${url}`
    const target = hostPortFromUrl(normalised)
    if (!target) {
      error = m.nextcloud_connect_error_parse_url()
      return
    }
    try {
      const probed = await api.accounts.probeServerCertificate({
        host: target.host,
        port: target.port,
      })
      pendingCert = probed
    } catch (e: any) {
      error = m.nextcloud_connect_cert_probe_failed({
        reason: formatError(e) || m.nextcloud_connect_unknown_error(),
      })
    }
  }

  function trustPendingNcCert() {
    if (!pendingCert) return
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
    // Retry the connect now that the chain is trusted.
    void startConnect()
  }

  function dismissPendingNcCert() {
    pendingCert = null
  }

  async function startConnect() {
    error = ''
    const url = serverInput.trim()
    if (!url) {
      error = m.nextcloud_connect_error_empty_url()
      return
    }
    // Normalise: tolerate "cloud.example.com" by assuming https. NC
    // never supports plain http in practice, so we don't add that path.
    const normalised = /^https?:\/\//.test(url) ? url : `https://${url}`

    connecting = true
    try {
      const init = await api.nextcloud.startNextcloudLogin({
        serverUrl: normalised,
        // Pass the trust list so a self-signed server gets a clean
        // handshake on the second attempt after the user trusted the
        // cert via the prompt.
        trustedCerts: trustedCerts.length > 0 ? trustedCerts : null,
      })
      // Fire-and-forget the browser open — if it fails the user can
      // copy the URL manually from the fallback shown below.
      try {
        await api.system.openUrl({ url: init.login_url })
      } catch (e) {
        console.warn('open_url failed, user must open manually', e)
      }
      pendingLoginUrl = init.login_url
      beginPolling(init)
    } catch (e) {
      const msg = formatError(e) || m.nextcloud_connect_error_start_failed()
      // TLS trust failure → kick off the probe + cert prompt.
      // Anything else falls through as a regular error toast.
      if (looksLikeCertError(msg)) {
        connecting = false
        await handleNcCertError()
        return
      }
      error = msg
      connecting = false
    }
  }

  function beginPolling(init: LoginFlowInit) {
    // 2-second cadence is a compromise between UI responsiveness and
    // not hammering the NC server. Login Flow v2 tokens live for ~20
    // minutes; we stop on success, cancel, or any unexpected error.
    pollTimer = window.setInterval(async () => {
      try {
        const result = await api.nextcloud.pollNextcloudLogin({
          pollEndpoint: init.poll_endpoint,
          pollToken: init.poll_token,
          // Same trust list rides the polling call so the
          // post-success `fetch_capabilities` probe and the saved
          // account record both pick it up (#253).
          trustedCerts: trustedCerts.length > 0 ? trustedCerts : null,
        })
        if (result) {
          stopPolling()
          connecting = false
          pendingLoginUrl = ''
          serverInput = ''
          onconnected(result)
        }
      } catch (e) {
        stopPolling()
        connecting = false
        pendingLoginUrl = ''
        error = formatError(e) || m.nextcloud_connect_error_login_failed()
      }
    }, 2000)
  }

  function stopPolling() {
    if (pollTimer !== null) {
      window.clearInterval(pollTimer)
      pollTimer = null
    }
  }

  function cancelConnect() {
    // The server-side token just expires on its own — nothing to tell
    // Nextcloud. Local teardown is enough.
    stopPolling()
    connecting = false
    pendingLoginUrl = ''
  }
</script>

{#if error}
  <div class="text-sm text-red-500 p-3 bg-red-500/10 rounded-lg mb-3">{error}</div>
{/if}

{#if !connecting}
  <div>
    <label class="text-xs text-surface-500 block mb-1" for="nc-connect-server">
      {m.nextcloud_connect_server_url_label()}
    </label>
    <div class="flex gap-2">
      <input
        id="nc-connect-server"
        class="input flex-1 px-3 py-2 text-sm rounded-lg"
        placeholder="https://cloud.example.com"
        bind:value={serverInput}
        onkeydown={(e) => e.key === 'Enter' && startConnect()}
      />
      <button class="btn preset-filled-primary-500" onclick={startConnect}>
        {m.nextcloud_connect_button_connect()}
      </button>
    </div>

    <!-- Self-signed cert trust prompt (#253).  Same shape as
         AccountSetup's IMAP-side prompt: full chain SHA-256
         fingerprints, trust / cancel CTA. -->
    {#if pendingCert}
      <div class="mt-4 p-4 rounded-lg border border-warning-500/40 bg-warning-500/5">
        <p class="text-sm font-medium mb-1">
          {m.nextcloud_connect_cert_title()}
        </p>
        <p class="text-xs text-surface-500 mb-3">
          {m.nextcloud_connect_cert_explainer()}
        </p>
        <p class="text-xs mb-1">
          <span class="text-surface-500">{m.nextcloud_connect_cert_host_label()}</span>
          <span class="font-mono">{pendingCert.host}</span>
        </p>
        <div class="text-xs mb-3">
          <p class="text-surface-500 mb-1">
            {#if pendingCert.chain.length === 1}
              {m.nextcloud_connect_cert_fingerprint_one()}
            {:else}
              {m.nextcloud_connect_cert_fingerprint_many({ n: pendingCert.chain.length - 1 })}
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
            onclick={trustPendingNcCert}
          >{m.nextcloud_connect_cert_button_trust()}</button>
          <button
            type="button"
            class="btn btn-sm preset-outlined-surface-500"
            onclick={dismissPendingNcCert}
          >{m.nextcloud_connect_cert_button_cancel()}</button>
        </div>
      </div>
    {/if}

    {#if trustedCerts.length > 0 && !pendingCert}
      <div class="mt-3 p-3 rounded-lg border border-success-500/30 bg-success-500/5 text-xs text-surface-600 dark:text-surface-400 flex items-center gap-2">
        <Icon name="verified" size={14} />
        {#if trustedCerts.length === 1}
          {m.nextcloud_connect_trusting_one()}
        {:else}
          {m.nextcloud_connect_trusting_many({ n: trustedCerts.length })}
        {/if}
      </div>
    {/if}
  </div>
{:else}
  <!-- Waiting for browser auth -->
  <div class="space-y-2">
    <p class="text-sm flex items-center gap-2">
      <Icon name="loading" size={14} />
      {m.nextcloud_connect_waiting()}
    </p>
    {#if pendingLoginUrl}
      <p class="text-xs text-surface-500">
        {m.nextcloud_connect_waiting_fallback()}
        <a class="underline text-primary-500 break-all" href={pendingLoginUrl} target="_blank" rel="noopener">
          {pendingLoginUrl}
        </a>
      </p>
    {/if}
    <button class="btn btn-sm preset-outlined-surface-500" onclick={cancelConnect}>
      {m.nextcloud_connect_button_cancel()}
    </button>
  </div>
{/if}

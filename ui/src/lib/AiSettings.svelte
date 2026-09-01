<script lang="ts">
  /**
   * AI settings — the control surface for the local MCP server
   * (#439, design note on #419).
   *
   * Unkai never ships or calls an LLM itself: the MCP server
   * exposes the user's mail and groupware to whatever MCP-capable
   * agent *they* bring (BYO model).  This page is the only place
   * that interface is controlled from:
   *
   *   1. Master toggle — starts/stops the localhost listener
   *      (`AppSettings::mcp_enabled`; the backend reconciles the
   *      running server inside `update_app_settings`).
   *   2. Token management — generate/rotate/revoke the bearer
   *      token.  The secret is shown exactly once, right after
   *      generation; afterwards only a "token exists" bool is
   *      available from the backend.
   *   3. Client config snippet — ready-to-paste MCP client JSON
   *      (endpoint URL + Authorization header).
   *   4. Per-tool toggles — the page renders whatever
   *      `mcp_list_tools` advertises, grouped by category, so the
   *      tool surfaces landing in #440/#441 appear here without
   *      another frontend change.  Write tools carry warning
   *      hints because some groupware writes are themselves
   *      outbound channels (creating an event mails invites).
   *   5. Encrypted-content policy — default off: E2E-encrypted
   *      bodies stay redacted from tool responses.
   *
   * Persistence note: `update_app_settings` replaces the whole
   * settings struct, so every mutation here re-fetches the live
   * settings first and edits only the MCP fields — otherwise a
   * stale copy would silently roll back changes made from other
   * panels.  The `onsettingschanged` callback lets the parent
   * (AccountSettings) refresh its own copy for the same reason.
   */

  import * as api from './api'
  import { formatError } from './errors'
  import { notifySettingsChanged } from './settingsBundle'
  import Icon from './Icon.svelte'
  import Toggle from './Toggle.svelte'
  import { m } from '../paraglide/messages'

  /** The MCP slice of the Rust `AppSettings` struct.  The index
   *  signature keeps every other field intact on the round-trip —
   *  we always save the full object we fetched. */
  interface AppSettingsMcp {
    mcp_enabled: boolean
    mcp_port: number
    mcp_tool_enablement: Record<string, boolean>
    mcp_expose_decrypted_content: boolean
    [key: string]: unknown
  }
  /** Mirrors `unkai_mcp::McpServerStatus` (snake_case wire shape). */
  interface McpServerStatus {
    running: boolean
    port: number | null
    endpoint: string | null
    last_error: string | null
  }
  /** Mirrors the `McpToolView` rows from `mcp_list_tools`. */
  interface McpTool {
    id: string
    category: string
    access: 'read' | 'write'
    description: string
    enabled: boolean
  }

  interface Props {
    /** Fired after every successful save with the fresh settings
     *  object, so the parent can update its own cached copy
     *  instead of later overwriting ours with stale MCP fields. */
    onsettingschanged?: (settings: AppSettingsMcp) => void
  }
  let { onsettingschanged }: Props = $props()

  let settings = $state<AppSettingsMcp | null>(null)
  let status = $state<McpServerStatus | null>(null)
  let tools = $state<McpTool[]>([])
  let hasToken = $state(false)
  let loading = $state(true)
  let saving = $state(false)
  let generating = $state(false)
  let error = $state('')

  /** The freshly-generated bearer token.  Lives only in this
   *  component's memory while the panel is open — the backend
   *  never returns it again after `mcp_generate_token`. */
  let generatedToken = $state<string | null>(null)

  /** Raw string state for the port input so a half-typed value
   *  doesn't fight the number parsing. */
  let portRaw = $state('')

  const enabled = $derived(settings?.mcp_enabled ?? false)

  // ── Category grouping ────────────────────────────────────────
  // Known categories get a fixed order + localized label; anything
  // the registry adds beyond these is appended with its raw id so
  // a new backend category never renders an empty page.
  const CATEGORY_ORDER = ['mail', 'contacts', 'calendar', 'talk', 'server']
  const CATEGORY_LABELS: Record<string, () => string> = {
    mail: m.settings_ai_cat_mail,
    contacts: m.settings_ai_cat_contacts,
    calendar: m.settings_ai_cat_calendar,
    talk: m.settings_ai_cat_talk,
    server: m.settings_ai_cat_server,
  }
  /** Warning hints for write tools whose side effects reach other
   *  people (see the #419 security model).  Keyed by stable tool
   *  id; the ids land with #440/#441 and pick their hint up here. */
  const WRITE_TOOL_HINTS: Record<string, () => string> = {
    create_draft: m.settings_ai_hint_create_draft,
    create_contact: m.settings_ai_hint_create_contact,
    create_event: m.settings_ai_hint_create_event,
    rsvp_event: m.settings_ai_hint_rsvp_event,
    create_talk_room: m.settings_ai_hint_create_talk_room,
    create_meeting_invite: m.settings_ai_hint_create_meeting_invite,
  }
  const groupedTools = $derived.by(() => {
    const byCategory = new Map<string, McpTool[]>()
    for (const tool of tools) {
      const list = byCategory.get(tool.category) ?? []
      list.push(tool)
      byCategory.set(tool.category, list)
    }
    const order = [
      ...CATEGORY_ORDER,
      ...[...byCategory.keys()].filter((c) => !CATEGORY_ORDER.includes(c)).sort(),
    ]
    return order
      .filter((c) => byCategory.has(c))
      .map((c) => ({
        category: c,
        label: CATEGORY_LABELS[c]?.() ?? c,
        tools: byCategory.get(c)!,
      }))
  })

  // ── Client config snippet ────────────────────────────────────
  // Uses the live endpoint while the server runs, otherwise the
  // address it *would* bind, so the snippet is correct either way.
  const endpointUrl = $derived(
    status?.endpoint ?? `http://127.0.0.1:${settings?.mcp_port ?? ''}/mcp`,
  )
  const configSnippet = $derived(
    JSON.stringify(
      {
        mcpServers: {
          'unkai-mail': {
            type: 'http',
            url: endpointUrl,
            headers: { Authorization: `Bearer ${generatedToken ?? 'YOUR_TOKEN'}` },
          },
        },
      },
      null,
      2,
    ),
  )

  // ── Loading ──────────────────────────────────────────────────
  async function loadAll() {
    loading = true
    try {
      const [fetchedSettings, fetchedStatus, fetchedToken, fetchedTools] =
        await Promise.all([
          api.settings.getAppSettings(),
          api.settings.mcpServerStatus(),
          api.settings.mcpTokenStatus(),
          api.settings.mcpListTools(),
        ])
      settings = fetchedSettings
      status = fetchedStatus
      hasToken = fetchedToken
      tools = fetchedTools
      portRaw = String(fetchedSettings.mcp_port)
    } catch (e) {
      error = formatError(e) || m.settings_ai_error_load()
    } finally {
      loading = false
    }
  }
  $effect(() => {
    void loadAll()
  })

  async function refreshStatus() {
    try {
      status = await api.settings.mcpServerStatus()
    } catch (e) {
      console.warn('mcp_server_status failed', e)
    }
  }

  async function refreshTools() {
    try {
      tools = await api.settings.mcpListTools()
    } catch (e) {
      console.warn('mcp_list_tools failed', e)
    }
  }

  // ── Saving ───────────────────────────────────────────────────
  /** Re-fetch the live settings, apply one MCP mutation, save the
   *  whole object back.  The re-fetch is what keeps this panel
   *  from clobbering changes made elsewhere since our last load. */
  async function saveMcp(mutate: (s: AppSettingsMcp) => void) {
    if (saving) return
    saving = true
    error = ''
    try {
      const fresh = await api.settings.getAppSettings()
      mutate(fresh)
      await api.settings.updateAppSettings({ newSettings: fresh })
      settings = fresh
      onsettingschanged?.(fresh)
      // Kick the Nextcloud settings-sync worker (#168) — the MCP
      // prefs travel in the bundle (the token never does).
      void notifySettingsChanged()
    } catch (e) {
      error = formatError(e) || m.settings_ai_error_save()
    } finally {
      saving = false
    }
  }

  async function setMcpEnabled(v: boolean) {
    await saveMcp((s) => {
      s.mcp_enabled = v
    })
    // `update_app_settings` reconciles the server before it
    // returns, so the status right after is already accurate.
    await refreshStatus()
  }

  async function onPortChange() {
    const parsed = parseInt(portRaw, 10)
    if (!Number.isFinite(parsed) || parsed < 1024 || parsed > 65535) {
      // Out of range or not a number — snap back to the saved
      // value rather than persisting something the bind would
      // reject anyway.
      portRaw = String(settings?.mcp_port ?? '')
      return
    }
    if (parsed === settings?.mcp_port) return
    await saveMcp((s) => {
      s.mcp_port = parsed
    })
    await refreshStatus()
  }

  async function setToolEnabled(tool: McpTool, v: boolean) {
    await saveMcp((s) => {
      s.mcp_tool_enablement = { ...s.mcp_tool_enablement, [tool.id]: v }
    })
    // Re-read from the backend so the rows always show the
    // *effective* enablement the server will enforce.
    await refreshTools()
  }

  async function setExposeDecrypted(v: boolean) {
    await saveMcp((s) => {
      s.mcp_expose_decrypted_content = v
    })
  }

  // ── Token management ─────────────────────────────────────────
  async function generateToken() {
    if (generating) return
    if (hasToken && !confirm(m.settings_ai_token_regenerate_confirm())) return
    generating = true
    error = ''
    try {
      generatedToken = await api.settings.mcpGenerateToken()
      hasToken = true
    } catch (e) {
      error = formatError(e) || m.settings_ai_error_save()
    } finally {
      generating = false
    }
  }

  async function revokeToken() {
    if (!confirm(m.settings_ai_token_revoke_confirm())) return
    error = ''
    try {
      await api.settings.mcpRevokeToken()
      hasToken = false
      generatedToken = null
    } catch (e) {
      error = formatError(e) || m.settings_ai_error_save()
    }
  }

  // ── Copy-to-clipboard with a transient "copied" tick ─────────
  // Same shape as SharesView: flip the `copy` icon to `success`
  // for a moment instead of swapping button labels.
  let copiedTarget = $state<'token' | 'snippet' | null>(null)
  let copiedTimer: number | null = null
  async function copyText(target: 'token' | 'snippet', text: string) {
    try {
      await navigator.clipboard.writeText(text)
      copiedTarget = target
      if (copiedTimer) window.clearTimeout(copiedTimer)
      copiedTimer = window.setTimeout(() => {
        copiedTarget = null
      }, 1600)
    } catch (e) {
      error = formatError(e) || m.settings_ai_error_save()
    }
  }
</script>

<section class="space-y-6">
  <header>
    <h2 class="text-xl font-semibold">{m.settings_ai_title()}</h2>
    <p class="text-sm text-surface-500 mt-1 max-w-xl">
      {m.settings_ai_intro()}
    </p>
  </header>

  {#if loading}
    <p class="text-sm text-surface-500">{m.settings_ai_loading()}</p>
  {:else if settings}
    <!-- Master toggle.  Same shape as the Key Encryption toggle in
         SecuritySettings: switch left, label column right, badge
         when active / hint copy when off.  Everything below greys
         out while the interface is disabled. -->
    <div class="flex items-center gap-3">
      <Toggle
        checked={enabled}
        disabled={saving}
        label={m.settings_ai_master_label()}
        onchange={(v) => void setMcpEnabled(v)}
      />
      <div class="max-w-xl">
        <p class="font-medium leading-tight">{m.settings_ai_master_label()}</p>
        {#if enabled && status?.running && status.endpoint}
          <div
            class="inline-flex items-center gap-1 mt-1 text-xs text-success-500"
            aria-live="polite"
          >
            <Icon name="success" size={14} />
            <span>{m.settings_ai_master_running({ endpoint: status.endpoint })}</span>
          </div>
        {:else if enabled && status?.last_error}
          <p class="text-xs text-error-500 leading-snug mt-1">
            {m.settings_ai_master_error({ error: status.last_error })}
          </p>
        {:else if enabled}
          <p class="text-xs text-surface-500 leading-snug mt-1">
            {m.settings_ai_master_starting()}
          </p>
        {:else}
          <p class="text-xs text-surface-500 leading-snug mt-1">
            {m.settings_ai_master_hint_off()}
          </p>
        {/if}
      </div>
    </div>

    <div
      class="space-y-4 transition-opacity {enabled
        ? ''
        : 'opacity-50 pointer-events-none select-none'}"
      aria-disabled={!enabled}
    >
      <!-- Port.  Kept in the fixed 1024–65535 range — MCP clients
           want a stable address, so the ephemeral-port trick (0)
           stays a test-only affair. -->
      <div class="rounded-lg border border-surface-200 dark:border-surface-700 p-4">
        <div class="flex items-center gap-3">
          <label class="text-sm text-surface-700 dark:text-surface-300" for="mcp-port">
            {m.settings_ai_port_label()}
          </label>
          <input
            id="mcp-port"
            type="number"
            min="1024"
            max="65535"
            class="input w-32 text-sm px-3 py-1.5 rounded-lg"
            bind:value={portRaw}
            onchange={() => void onPortChange()}
            disabled={saving || !enabled}
          />
        </div>
        <p class="text-xs text-surface-500 mt-1">{m.settings_ai_port_hint()}</p>
      </div>

      <!-- Token management.  The secret is displayed exactly once,
           straight from `mcp_generate_token`; afterwards the
           backend only reports whether one exists. -->
      <div class="rounded-lg border border-surface-200 dark:border-surface-700 p-4 space-y-3">
        <div class="flex items-center gap-3">
          <div class="flex-1 min-w-0">
            <h3 class="font-medium leading-tight">{m.settings_ai_token_title()}</h3>
            {#if hasToken}
              <div
                class="inline-flex items-center gap-1 mt-1 text-xs text-success-500"
                aria-live="polite"
              >
                <Icon name="success" size={14} />
                <span>{m.settings_ai_token_status_present()}</span>
              </div>
            {:else}
              <p class="text-xs text-surface-500 leading-snug mt-1">
                {m.settings_ai_token_status_absent()}
              </p>
            {/if}
          </div>
          <button
            class="btn btn-sm {hasToken
              ? 'preset-outlined-surface-500'
              : 'preset-filled-primary-500'} inline-flex items-center gap-1.5"
            disabled={generating || !enabled}
            onclick={() => void generateToken()}
          >
            <Icon name={generating ? 'loading' : hasToken ? 'refresh' : 'plus'} size={14} />
            <span>
              {hasToken ? m.settings_ai_token_regenerate() : m.settings_ai_token_generate()}
            </span>
          </button>
          {#if hasToken}
            <button
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-error-500/15 hover:text-error-500 hover:border-error-500/40"
              disabled={generating || !enabled}
              title={m.settings_ai_token_revoke()}
              aria-label={m.settings_ai_token_revoke()}
              onclick={() => void revokeToken()}
            ><Icon name="trash" size={14} /></button>
          {/if}
        </div>

        {#if generatedToken}
          <div class="rounded-lg border border-surface-200 dark:border-surface-700 bg-surface-100 dark:bg-surface-800 p-3">
            <div class="flex items-center gap-2">
              <code class="flex-1 min-w-0 font-mono text-xs break-all select-all">
                {generatedToken}
              </code>
              <button
                class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center shrink-0"
                title={copiedTarget === 'token'
                  ? m.settings_ai_copied()
                  : m.settings_ai_copy()}
                aria-label={copiedTarget === 'token'
                  ? m.settings_ai_copied()
                  : m.settings_ai_copy()}
                onclick={() => void copyText('token', generatedToken ?? '')}
              ><Icon name={copiedTarget === 'token' ? 'success' : 'copy'} size={14} /></button>
            </div>
            <p class="text-xs text-warning-600 dark:text-warning-400 mt-2">
              {m.settings_ai_token_secret_note()}
            </p>
          </div>
        {/if}

        <p class="text-xs text-surface-500">{m.settings_ai_token_hint()}</p>
      </div>

      <!-- Ready-to-paste MCP client config. -->
      <div class="rounded-lg border border-surface-200 dark:border-surface-700 p-4 space-y-2">
        <div class="flex items-center gap-3">
          <h3 class="flex-1 min-w-0 font-medium leading-tight">
            {m.settings_ai_snippet_title()}
          </h3>
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
            title={copiedTarget === 'snippet' ? m.settings_ai_copied() : m.settings_ai_copy()}
            aria-label={copiedTarget === 'snippet'
              ? m.settings_ai_copied()
              : m.settings_ai_copy()}
            onclick={() => void copyText('snippet', configSnippet)}
          ><Icon name={copiedTarget === 'snippet' ? 'success' : 'copy'} size={14} /></button>
        </div>
        <pre
          class="rounded-lg bg-surface-100 dark:bg-surface-800 p-3 text-xs font-mono overflow-x-auto"><code
            >{configSnippet}</code></pre>
        <p class="text-xs text-surface-500">{m.settings_ai_snippet_hint()}</p>
      </div>

      <!-- Per-tool toggles, grouped by category.  The list is
           whatever the registry advertises — the toggle writes an
           explicit entry into `mcp_tool_enablement`, which the
           server re-checks on every `tools/call`. -->
      <div class="space-y-3">
        <div>
          <h3 class="font-medium">{m.settings_ai_tools_title()}</h3>
          <p class="text-xs text-surface-500 mt-1 max-w-xl">{m.settings_ai_tools_hint()}</p>
        </div>
        {#if tools.length === 0}
          <p class="text-sm text-surface-500 italic">{m.settings_ai_tools_empty()}</p>
        {:else}
          {#each groupedTools as group (group.category)}
            <div>
              <h4 class="text-xs font-semibold uppercase tracking-wide text-surface-500 mb-1.5">
                {group.label}
              </h4>
              <ul class="divide-y divide-surface-200 dark:divide-surface-700 rounded-lg border border-surface-200 dark:border-surface-700">
                {#each group.tools as tool (tool.id)}
                  <li class="flex items-start gap-3 p-3">
                    <Toggle
                      checked={tool.enabled}
                      disabled={saving || !enabled}
                      label={tool.id}
                      onchange={(v) => void setToolEnabled(tool, v)}
                      class="mt-0.5"
                    />
                    <div class="flex-1 min-w-0">
                      <div class="flex items-center gap-2">
                        <code class="font-mono text-sm">{tool.id}</code>
                        <span
                          class="text-[10px] uppercase tracking-wide px-1.5 py-0.5 rounded {tool.access ===
                          'write'
                            ? 'bg-warning-500/15 text-warning-600 dark:text-warning-400'
                            : 'bg-surface-200 dark:bg-surface-700 text-surface-600 dark:text-surface-300'}"
                        >
                          {tool.access === 'write'
                            ? m.settings_ai_tools_write_badge()
                            : m.settings_ai_tools_read_badge()}
                        </span>
                      </div>
                      <p class="text-xs text-surface-500 mt-0.5">{tool.description}</p>
                      {#if tool.access === 'write' && WRITE_TOOL_HINTS[tool.id]}
                        <p class="text-xs text-warning-600 dark:text-warning-400 mt-0.5">
                          {WRITE_TOOL_HINTS[tool.id]()}
                        </p>
                      {/if}
                    </div>
                  </li>
                {/each}
              </ul>
            </div>
          {/each}
        {/if}
      </div>

      <!-- Encrypted-content policy.  Default off: end-to-end
           encrypted bodies stay redacted from every tool response
           (#440 enforces it); this is the explicit opt-out. -->
      <div class="rounded-lg border border-surface-200 dark:border-surface-700 p-4">
        <div class="flex items-start gap-3">
          <Toggle
            checked={settings.mcp_expose_decrypted_content}
            disabled={saving || !enabled}
            label={m.settings_ai_encrypted_label()}
            onchange={(v) => void setExposeDecrypted(v)}
            class="mt-0.5"
          />
          <div class="max-w-xl">
            <p class="font-medium leading-tight">{m.settings_ai_encrypted_label()}</p>
            {#if settings.mcp_expose_decrypted_content}
              <p class="text-xs text-warning-600 dark:text-warning-400 leading-snug mt-1">
                {m.settings_ai_encrypted_hint_on()}
              </p>
            {:else}
              <p class="text-xs text-surface-500 leading-snug mt-1">
                {m.settings_ai_encrypted_hint_off()}
              </p>
            {/if}
          </div>
        </div>
      </div>
    </div>

    {#if error}
      <p class="text-sm text-error-500 wrap-break-word">{error}</p>
    {/if}
  {/if}
</section>

<script lang="ts">
  /**
   * StandaloneMailFile — view-only popout for an .eml file (#254).
   *
   * Mounted by main.ts when the URL carries
   * `?view=mailfile&path=...`.  The path points at a local .eml on
   * disk (typically supplied by the OS via "Open with… → Unkai").
   * We hand the path to the `parse_eml_file` Tauri command, which
   * runs the same MIME walker `fetch_message` uses internally and
   * returns the same Email shape.  No IMAP, no cache — just bytes
   * from disk turned into a renderable message.
   *
   * Read-only by design: the file isn't tied to any account or
   * folder, so reply / forward / archive don't have a well-defined
   * destination.  Subject + headers + body, sanitised HTML, that's
   * the entire UI.  Closing the window is the only outbound action.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import DOMPurify from 'dompurify'
  import { onMount, onDestroy } from 'svelte'
  import Icon from './Icon.svelte'
  import { applyTheme, installSystemModeListener, type ThemeMode } from './theme'
  import { formatError } from './errors'

  interface EmailAttachment {
    filename: string
    content_type: string
    size: number | null
    part_id: number
    content_id?: string | null
  }

  interface ParsedEmail {
    id: string
    account_id: string
    folder: string
    from: string
    to: string[]
    cc: string[]
    subject: string
    body_text: string | null
    body_html: string | null
    date: string
    is_read: boolean
    is_starred: boolean
    has_attachments: boolean
    attachments: EmailAttachment[]
  }

  let { path }: { path: string } = $props()

  let email = $state<ParsedEmail | null>(null)
  let loading = $state(true)
  let loadError = $state('')
  let unlistenSystemMode: (() => void) | null = null

  onMount(() => {
    void (async () => {
      try {
        const prefs = await invoke<{
          theme_name: string
          theme_mode: ThemeMode
        }>('get_app_settings')
        applyTheme(prefs.theme_name, prefs.theme_mode)
        unlistenSystemMode = installSystemModeListener(
          prefs.theme_mode,
          prefs.theme_name,
        )
      } catch (e) {
        console.warn('get_app_settings failed in standalone mailfile', e)
      }

      try {
        email = await invoke<ParsedEmail>('parse_eml_file', { path })
      } catch (e) {
        loadError = formatError(e) || 'Could not parse the email file.'
      } finally {
        loading = false
      }
    })()
  })

  onDestroy(() => {
    unlistenSystemMode?.()
  })

  // Same allow-list as MailView's sanitiser, minus the per-message
  // image-blocking flow — a file the user explicitly opened from
  // disk is already trusted enough to load remote images, and
  // there's no per-sender trust state to read here anyway.  Scripts,
  // iframes, forms, and embedded stylesheet blocks are still
  // stripped so a hostile message can't mount a UI redress attack
  // against the popout chrome.
  function sanitiseHtml(html: string): string {
    const clean = DOMPurify.sanitize(html, {
      FORBID_TAGS: [
        'script',
        'noscript',
        'object',
        'embed',
        'applet',
        'iframe',
        'frame',
        'frameset',
        'form',
        'input',
        'textarea',
        'select',
        'button',
        'base',
        'meta',
        'link',
        'style',
      ],
      ADD_ATTR: ['target', 'title'],
      FORCE_BODY: true,
    })
    const doc = new DOMParser().parseFromString(clean, 'text/html')
    for (const a of Array.from(doc.querySelectorAll('a[href]'))) {
      a.setAttribute('target', '_blank')
      a.setAttribute('rel', 'noopener noreferrer')
    }
    return doc.body.innerHTML
  }

  let renderedHtml = $derived(
    email && email.body_html ? sanitiseHtml(email.body_html) : '',
  )

  function formatFullDate(iso: string): string {
    if (!iso) return ''
    const d = new Date(iso)
    if (Number.isNaN(d.getTime())) return iso
    return d.toLocaleString()
  }

  function closeWindow() {
    void getCurrentWindow().close()
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault()
      closeWindow()
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<svelte:head>
  <title>{email?.subject || 'Unkai Mail'}</title>
</svelte:head>

<div class="h-screen flex flex-col bg-surface-50 dark:bg-surface-900 text-surface-900 dark:text-surface-100">
  {#if loading}
    <div class="flex-1 flex items-center justify-center text-sm text-surface-500">
      Loading…
    </div>
  {:else if loadError}
    <div class="flex-1 flex items-center justify-center px-6">
      <div class="max-w-md text-center space-y-2">
        <div class="text-error-500"><Icon name="warning" size={32} /></div>
        <div class="text-base font-semibold">Could not open this email file</div>
        <div class="text-sm text-surface-500 wrap-break-word">{loadError}</div>
        <button type="button" class="btn btn-sm preset-outlined-surface-500 mt-3" onclick={closeWindow}>
          Close
        </button>
      </div>
    </div>
  {:else if email}
    <header class="px-6 pt-5 pb-4 border-b border-surface-300/60 dark:border-surface-700/60">
      <h1 class="text-lg font-semibold leading-snug wrap-break-word">
        {email.subject || '(no subject)'}
      </h1>
      <dl class="mt-3 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
        <dt class="text-surface-500">From</dt>
        <dd class="wrap-break-word">{email.from || '(unknown sender)'}</dd>
        {#if email.to.length > 0}
          <dt class="text-surface-500">To</dt>
          <dd class="wrap-break-word">{email.to.join(', ')}</dd>
        {/if}
        {#if email.cc.length > 0}
          <dt class="text-surface-500">Cc</dt>
          <dd class="wrap-break-word">{email.cc.join(', ')}</dd>
        {/if}
        {#if email.date}
          <dt class="text-surface-500">Date</dt>
          <dd>{formatFullDate(email.date)}</dd>
        {/if}
      </dl>
      {#if email.attachments.length > 0}
        <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-surface-500">
          <span class="flex items-center gap-1">
            <Icon name="attachment" size={12} />
            {email.attachments.length} attachment{email.attachments.length === 1 ? '' : 's'}
          </span>
          {#each email.attachments as a}
            <span class="px-2 py-0.5 rounded-full bg-surface-200 dark:bg-surface-800 wrap-break-word">
              {a.filename || a.content_type}
            </span>
          {/each}
        </div>
      {/if}
    </header>

    <section class="flex-1 overflow-auto">
      {#if renderedHtml}
        <div class="px-6 py-4 mail-body" style="background:white;color:#111">
          {@html renderedHtml}
        </div>
      {:else if email.body_text}
        <pre class="px-6 py-4 text-sm whitespace-pre-wrap wrap-break-word font-mono">{email.body_text}</pre>
      {:else}
        <div class="px-6 py-4 text-sm text-surface-500 italic">
          (This message has no displayable body.)
        </div>
      {/if}
    </section>
  {/if}
</div>

<style>
  .mail-body :global(img) {
    max-width: 100%;
    height: auto;
  }
  .mail-body :global(table) {
    max-width: 100%;
  }
</style>

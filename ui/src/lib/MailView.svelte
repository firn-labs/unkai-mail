<script lang="ts">
  /**
   * MailView — the right-hand reading pane.
   *
   * Given an account + folder + UID, calls `fetch_message` to pull the
   * full message (headers + body) from the IMAP server. Renders plain
   * text if we have it, otherwise falls back to inline-rendered HTML
   * sanitized by DOMPurify (scripts and dangerous attributes stripped)
   * with remote images blocked by default.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { save } from '@tauri-apps/plugin-dialog'
  import DOMPurify from 'dompurify'
  import { formatError } from './errors'
  import { m } from '../paraglide/messages'
  import NextcloudFilePicker from './NextcloudFilePicker.svelte'
  import MoveFolderPicker from './MoveFolderPicker.svelte'
  import FileTypeIcon from './FileTypeIcon.svelte'
  import Icon from './Icon.svelte'
  import AttachmentThumb, { seedThumbFromBase64 } from './AttachmentThumb.svelte'
  import CalendarInviteCard, { type InviteSummary } from './CalendarInviteCard.svelte'
  import { openMailInStandaloneWindow } from './standaloneMailWindow'
  import {
    isMarkdownAttachment,
    isOfficeAttachment,
    isPdfAttachment,
    openAttachment,
  } from './attachmentOpen'
  import {
    addTrustedSender,
    getSenderAddress,
    isSenderTrusted,
  } from './trustedSenders'

  interface EmailAttachment {
    filename: string
    content_type: string
    size: number | null
    /**
     * Stable index of this part inside the original MIME tree, used by
     * the backend to re-fetch and extract just this attachment's bytes
     * without retransmitting the rest of the message.
     */
    part_id: number
    /** RFC 2392 Content-ID — present on attachments referenced inline
     *  via `<a href="cid:…">` in the body. Used to route those
     *  anchor clicks to the right attachment in `attachmentClicked`. */
    content_id?: string | null
  }

  interface Email {
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

  interface Props {
    accountId: string
    folder?: string
    uid: number | null
    onread?: (uid: number) => void
    onreply?: (mail: Email & { uid: number }) => void
    onreplyall?: (mail: Email & { uid: number }) => void
    onforward?: (mail: Email & { uid: number }) => void
    /** Open the "Create Talk room" flow seeded from this email's
        subject + thread participants. Wired from `App.svelte` so the
        resulting Compose window stacks on top of the inbox view. */
    onrespondwithmeeting?: (mail: Email & { uid: number }) => void
    /** Save the email as a Nextcloud note. The handler in
        `App.svelte` picks the NC account and POSTs to the Notes
        API — we just hand over the email so the title/body are
        sourced consistently with what's currently visible. */
    onsavenote?: (mail: Email) => void
    /** True when the currently selected folder is the account's
        Drafts mailbox. Swaps the toolbar over from the reply/forward
        cluster to a single "Edit" action, because Reply-to-a-draft
        doesn't model anything useful. */
    isDraftsFolder?: boolean
    /** True when the currently selected folder is the account's
        Sent mailbox.  Used to suppress the iMIP RSVP card for
        invites the user themselves sent — you don't reply to your
        own meeting requests, and showing Accept/Decline on a
        message in Sent is misleading. */
    isSentFolder?: boolean
    /** Open the shown draft back in Compose for editing. Fires only
        from the "Edit" button inside the drafts toolbar. */
    oneditdraft?: (mail: Email) => void
    /** Fires after the message has been successfully archived or
        deleted on the server.  The removed UID is passed back so the
        parent can compute the "next" message to open (auto-advance
        behaviour) instead of forcing the user back to the empty
        reading-pane state. */
    onmessageremoved?: (removedUid: number) => void
    /** App-wide default for "render HTML email on a white canvas".
        When true the body wrapper gets a forced white background and
        dark text so emails designed for a light page stay readable in
        dark mode. The user can override per message via a toolbar
        toggle. */
    forceWhiteBackground?: boolean
    /** App-wide opt-in (#197) for "always load remote images".
        When true, the per-message blocking pipeline is bypassed —
        every HTML mail renders with its remote images loaded and
        the "Remote images are blocked" banner doesn't appear.
        Trades the privacy default for convenience. */
    autoLoadRemoteImages?: boolean
    /** App-wide master toggle (#165) for the URLhaus link
     *  checker.  When true, every link in the rendered body
     *  gets a green "Safe" / red "Unsafe" pill, and clicks on
     *  unsafe links go through a confirm modal.  When false,
     *  links open without interception. */
    linkCheckEnabled?: boolean
    /** True when this `MailView` is the root of a popped-out
        standalone window (#104).  Hides the "Open in window"
        button (no point inside the window it would open) and
        otherwise behaves identically. */
    inStandaloneWindow?: boolean
    /** Open compose pre-filled with a recipient (and optional
        cc/bcc/subject/body) — used when the user clicks a
        `mailto:` link inside a rendered email. */
    onmailto?: (init: { to?: string; cc?: string; bcc?: string; subject?: string; body?: string }) => void
    /** Bindable refresh hint — see the equivalent prop on
     *  `MailList`.  The network re-fetch after the cache paint
     *  flips this true; `App.svelte` aggregates it with
     *  MailList's flag and shows a calm spinner on the
     *  IconRail's active-account avatar (#161). */
    refreshing?: boolean
    /** UIDs of every member of the thread the open message
     *  belongs to (#289 follow-up).  Populated only when the
     *  open message is the *head* of a multi-member thread
     *  (the row that carries the count badge in MailList); `null`
     *  otherwise.  When non-null, the archive button sweeps the
     *  whole conversation in a single batched IMAP move instead
     *  of archiving only the head row.  Move-to-folder is handled
     *  in MailList already (via `affectedEnvelopes`), so this
     *  prop only drives archive here. */
    threadMemberUids?: number[] | null
  }
  let {
    accountId,
    folder = 'INBOX',
    uid,
    onread,
    onreply,
    onreplyall,
    onforward,
    onrespondwithmeeting,
    onsavenote,
    isDraftsFolder = false,
    isSentFolder = false,
    oneditdraft,
    onmessageremoved,
    inStandaloneWindow = false,
    forceWhiteBackground = true,
    autoLoadRemoteImages = false,
    linkCheckEnabled = true,
    onmailto,
    refreshing = $bindable(false),
    threadMemberUids = null,
  }: Props = $props()

  let email = $state<Email | null>(null)
  let loading = $state(false)
  let error = $state('')

  /** Pre-fetch persisted thumbnails for one message and seed
   *  the in-memory thumb cache so AttachmentThumb's first
   *  mount hits straight away (no bytesProvider call, no
   *  codec activity).  Awaited from `load()` *before* `email`
   *  is assigned so the chip strip never mounts ahead of the
   *  cache. */
  async function seedAttachmentPreviews(
    acc: string,
    fld: string,
    u: number,
  ): Promise<void> {
    try {
      const rows = await invoke<{ partId: number; mime: string; base64: string }[]>(
        'get_attachment_previews',
        { accountId: acc, folder: fld, uid: u },
      )
      for (const r of rows) {
        seedThumbFromBase64({
          cacheKey: `${acc}::${fld}::${u}::${r.partId}`,
          mime: r.mime,
          base64: r.base64,
        })
      }
    } catch (e) {
      console.warn('get_attachment_previews failed', e)
    }
  }

  // ── Calendar invite (#58 / iMIP) ──────────────────────────────
  // Inbound mail carrying a `text/calendar` attachment surfaces an
  // RSVP card above the body with Accept / Decline / Tentative
  // buttons.  We detect the attachment by content-type, fetch its
  // bytes through the existing `download_email_attachment` path,
  // hand them to `parse_event_invite` for a slim summary, and
  // mount `CalendarInviteCard` with the result.
  let invite = $state<InviteSummary | null>(null)
  let inviteLoadError = $state('')

  /** Pick the first iCalendar-shaped attachment off the open
   *  mail.  Senders differ — different mail clients and calendar
   *  servers pick differently between `text/calendar`,
   *  `application/ics` and a generic Content-Type with an
   *  `.ics` filename — so we match all of them. */
  function pickInviteAttachment(em: Email | null): EmailAttachment | null {
    if (!em) return null
    return (
      em.attachments.find((a) => {
        const ct = a.content_type.toLowerCase()
        const fn = a.filename.toLowerCase()
        return (
          ct.startsWith('text/calendar') ||
          ct.startsWith('application/ics') ||
          ct.startsWith('application/ical') ||
          fn.endsWith('.ics') ||
          fn.endsWith('.ical') ||
          fn.endsWith('.icalendar')
        )
      }) ?? null
    )
  }

  $effect(() => {
    if (!email || uid == null) {
      invite = null
      inviteLoadError = ''
      return
    }
    const att = pickInviteAttachment(email)
    const cur = email
    const curUid = uid
    void (async () => {
      try {
        const bytes = att
          ? await invoke<number[]>('download_email_attachment', {
              accountId: cur.account_id,
              folder: cur.folder,
              uid: curUid,
              partId: att.part_id,
            })
          : await invoke<number[] | null>('download_calendar_from_message', {
              accountId: cur.account_id,
              folder: cur.folder,
              uid: curUid,
            })
        if (!bytes) {
          if (email === cur) {
            invite = null
            inviteLoadError = ''
          }
          return
        }
        // Race-guard: bail if the user navigated to a different
        // mail before our fetch completed.
        if (email !== cur) return
        const summary = await invoke<InviteSummary>('parse_event_invite', { bytes })
        if (email !== cur) return
        // Surface the card for `METHOD:REQUEST` (organiser-sent
        // invites — Accept / Tentative / Decline UI) and
        // `METHOD:CANCEL` (organiser-sent cancellations —
        // "Remove from my calendar" UI).  Other methods
        // (`REPLY`, `PUBLISH`, etc.) aren't actionable inbound
        // and would just be noise; we filter them out so the
        // mail body renders unobstructed.
        const m = summary.method?.toUpperCase()
        if (m && m !== 'REQUEST' && m !== 'CANCEL') {
          invite = null
          inviteLoadError = ''
          return
        }
        // Record CANCEL observations so the original REQUEST
        // mail's card can flip to the cancelled banner on its
        // next open and the user doesn't unwittingly answer a
        // meeting that's been cancelled.  Best-effort — a
        // persistence failure doesn't block the card mounting.
        if (m === 'CANCEL') {
          void invoke('record_cancelled_invite', { uid: summary.uid }).catch(
            (e) => console.warn('record_cancelled_invite failed', e),
          )
        }
        invite = summary
        inviteLoadError = ''
      } catch (e) {
        console.warn('parse_event_invite failed:', e)
        inviteLoadError = formatError(e) || 'Could not parse the calendar invite.'
        invite = null
      }
    })()
  })

  // Note: the RSVP card no longer needs an `accountEmail`
  // prop — `respond_to_invite` resolves the responding
  // address from NC's user profile server-side, which is what
  // Sabre uses internally to identify the responding attendee
  // anyway.  Single source of truth, no client-side guessing.

  $effect(() => {
    if (uid == null) {
      email = null
      return
    }
    void load(accountId, folder, uid)
  })

  async function load(id: string, f: string, u: number) {
    loading = true
    refreshing = false
    error = ''
    email = null
    showImagesForMessage = false
    trustedSender = false
    whiteBackgroundOverride = null

    // Cache first — lets the reading pane paint instantly when the user
    // re-opens a previously read message (the common case).
    try {
      const cached = await invoke<Email | null>('get_cached_message', {
        accountId: id,
        folder: f,
        uid: u,
      })
      if (id === accountId && f === folder && u === uid && cached) {
        // Resolve trust state BEFORE assigning `email`, otherwise the
        // first render of the message runs with trustedSender=false,
        // briefly flashes the "Remote images blocked" banner, and only
        // then settles into the trusted state — looks like a bug for
        // senders the user has already approved.
        trustedSender = isSenderTrusted(cached.from)
        // Seed the in-memory thumbnail cache (#157) *before*
        // assigning `email` — otherwise the chip strip mounts
        // first, AttachmentThumb's own effect kicks off
        // bytesProvider, and the seeded preview lands too late
        // to skip the work.  Fast: a single IPC + cheap
        // deserialise per attachment.
        await seedAttachmentPreviews(id, f, u)
        email = cached
        loading = false
      }
    } catch (e: any) {
      console.warn('get_cached_message failed:', e)
    }

    // Network refresh: pulls fresh flags / body in case the message
    // changed on the server (marked read elsewhere, updated draft, etc.).
    refreshing = email != null
    try {
      const fresh = await invoke<Email>('fetch_message', {
        accountId: id,
        folder: f,
        uid: u,
      })
      if (id === accountId && f === folder && u === uid) {
        trustedSender = isSenderTrusted(fresh.from)
        // Seed previews here too in case the cache miss path
        // (no prior `cached`) skipped the seeding above.
        await seedAttachmentPreviews(id, f, u)
        email = fresh
      }
    } catch (e: any) {
      // `MessageGone` (#288 sibling): the cached envelope is a
      // ghost — the message no longer exists on the server.  The
      // backend has already evicted the cache row and fired
      // `mail-flags-updated`; we hand the gone UID up so the parent
      // auto-advances to the next neighbour (App.svelte's
      // `onMessageRemoved` does the splice + selection move).  The
      // cached body, if any, is now misleading — clear it so the
      // user doesn't see "(deleted) reply to this thread" affordances
      // on a dead message.  Pane briefly shows the friendly note
      // until the parent picks the next UID.
      if (e === 'MessageGone') {
        email = null
        error = m.mail_view_message_gone()
        if (id === accountId && f === folder && u === uid) {
          onmessageremoved?.(u)
        }
      } else if (email == null) {
        error = formatError(e) || 'Failed to load message'
      } else {
        console.warn('fetch_message failed (showing cached):', e)
      }
    } finally {
      loading = false
      refreshing = false
    }

    // Mark as read — fire-and-forget. The MailList picked up an optimistic
    // cache update from the backend, and onread() lets the parent refresh
    // the envelope list so the unread styling clears immediately.
    if (email && !email.is_read && id === accountId && f === folder && u === uid) {
      try {
        await invoke('mark_as_read', { accountId: id, folder: f, uid: u })
        if (email) email.is_read = true
        onread?.(u)
      } catch (e: any) {
        console.warn('mark_as_read failed:', e)
      }
    }
  }

  /** Toggle the read state from the toolbar. Optimistic: flip the
      local flag so the button label flips immediately, then call the
      backend; revert if it fails. The parent's `onread` callback also
      fires so the mail list and sidebar badge update. */
  async function toggleRead() {
    if (!email || uid == null) return
    const next = !email.is_read
    email.is_read = next
    try {
      await invoke('set_message_read', {
        accountId,
        folder,
        uid,
        read: next,
      })
      onread?.(uid)
    } catch (e: any) {
      console.warn('set_message_read failed:', e)
      if (email) email.is_read = !next
    }
  }

  function formatFullDate(iso: string): string {
    return new Date(iso).toLocaleString()
  }

  // ── Per-sender image trust (persisted in localStorage) ──────────────
  // senders the user has chosen "always show images from" live here.
  // Key format: ["user@example.com", ...] — lower-cased bare addresses.

  // Per-message "Show images" toggle; reset to false on every new message.
  let showImagesForMessage = $state(false)
  // True when the sender is in the trusted list (set in load()).
  let trustedSender = $state(false)

  // Per-message override for the white-canvas default. `null` means
  // "use the app-wide preference"; `true` / `false` flip it just for
  // the open message. Reset on every new message in load().
  let whiteBackgroundOverride = $state<boolean | null>(null)
  let effectiveWhiteBackground = $derived(
    whiteBackgroundOverride ?? forceWhiteBackground,
  )

  // ── HTML sanitization + image blocking ───────────────────────────────
  //
  // DOMPurify strips scripts, event handlers, and any element that can
  // execute code or load external resources (iframe, object, form…).
  // We keep inline styles so newsletter formatting survives, but we
  // forbid <style> blocks — they can't be easily scoped and could
  // clobber the app's UI classes. Most real-world HTML email uses
  // inline styles anyway (most webmail strips <style> too, so senders know).
  //
  // After DOMPurify, a second pass with DOMParser:
  //   • annotates <a href> with a tooltip showing the raw URL (phishing
  //     guard — the Tauri webview hides the URL bar)
  //   • marks cid: anchors with data-nimbus-cid for the click handler
  //   • unless showImages is true, replaces every remote <img src> with
  //     a transparent 1×1 GIF and stashes the original in
  //     data-nimbus-blocked-src

  const BLOCKED_IMG_PLACEHOLDER =
    'data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7'

  /** Walk the body's text nodes and replace any naked http(s)
   *  URL with an `<a href>` so the link-check pass picks it up
   *  and so the user gets a clickable target (matches the
   *  behaviour of every modern mail client).  Skips text inside
   *  existing `<a>`, `<script>`, or `<style>` so we never
   *  nest anchors or rewrite code samples.  Trailing common
   *  punctuation (`.`, `,`, `)`, etc.) is stripped from the
   *  match so "see https://example.com." doesn't promote the
   *  sentence-final period into part of the URL. */
  function autolinkPlainTextUrls(doc: Document) {
    const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT, {
      acceptNode(node) {
        let p: Node | null = node.parentNode
        while (p) {
          if (p.nodeType === Node.ELEMENT_NODE) {
            const tag = (p as Element).tagName.toLowerCase()
            if (tag === 'a' || tag === 'script' || tag === 'style') {
              return NodeFilter.FILTER_REJECT
            }
          }
          p = p.parentNode
        }
        // `RegExp.test` mutates `lastIndex` on /g regexes, so
        // build a non-global probe just for the filter.
        return /https?:\/\/[^\s<>"]/i.test(node.textContent ?? '')
          ? NodeFilter.FILTER_ACCEPT
          : NodeFilter.FILTER_REJECT
      },
    })
    const targets: Text[] = []
    let n: Node | null
    while ((n = walker.nextNode())) targets.push(n as Text)
    if (targets.length === 0) return

    const urlRe = /(https?:\/\/[^\s<>"]+)/g
    for (const text of targets) {
      const content = text.textContent ?? ''
      urlRe.lastIndex = 0
      const fragment = doc.createDocumentFragment()
      let lastIndex = 0
      let match: RegExpExecArray | null
      while ((match = urlRe.exec(content)) !== null) {
        let url = match[0]
        let trailing = ''
        while (url.length > 0 && /[.,;:!?)\]]/.test(url[url.length - 1])) {
          trailing = url[url.length - 1] + trailing
          url = url.slice(0, -1)
        }
        if (url.length === 0) continue
        const start = match.index
        if (start > lastIndex) {
          fragment.appendChild(doc.createTextNode(content.slice(lastIndex, start)))
        }
        const a = doc.createElement('a')
        a.setAttribute('href', url)
        a.setAttribute('target', '_blank')
        a.setAttribute('rel', 'noopener noreferrer')
        a.textContent = url
        fragment.appendChild(a)
        if (trailing) fragment.appendChild(doc.createTextNode(trailing))
        lastIndex = start + url.length + trailing.length
      }
      if (lastIndex < content.length) {
        fragment.appendChild(doc.createTextNode(content.slice(lastIndex)))
      }
      text.parentNode?.replaceChild(fragment, text)
    }
  }

  function processEmailHtml(
    html: string,
    showImages: boolean,
  ): { html: string; hadBlocked: boolean } {
    if (!html) return { html: '', hadBlocked: false }
    try {
      const clean = DOMPurify.sanitize(html, {
        FORBID_TAGS: [
          'script', 'noscript', 'object', 'embed', 'applet',
          'iframe', 'frame', 'frameset',
          'form', 'input', 'textarea', 'select', 'button',
          'base', 'meta', 'link', 'style',
        ],
        ADD_ATTR: [
          'target',
          'data-nimbus-cid',
          'data-nimbus-blocked-src',
          'title',
          // Attachment-ref (#93) — survive sanitisation so the
          // body click handler can route the click back to the
          // matching attachment row.
          'data-attachment-ref',
          'data-cid',
          'data-filename',
          'data-label',
        ],
        FORCE_BODY: true,
      })

      const doc = new DOMParser().parseFromString(clean, 'text/html')

      // Auto-link plain-text URLs (#165 follow-up).  Many
      // senders put a URL straight into their message body as
      // plain text — Tiptap before its `autolink` config, most
      // CLI mailers, and any hand-written HTML that just embeds
      // the URL without wrapping it in <a>.  Without this pass
      // those URLs render as text and bypass the URLhaus link
      // check entirely (the extractor only walks `<a[href]>`).
      // We also annotate cid: / mailto: text-URLs?  No — only
      // http(s), since those are what URLhaus catalogues and
      // what the open-in-browser path handles.
      autolinkPlainTextUrls(doc)

      // Annotate links with tooltip + handle cid: anchors
      doc.querySelectorAll('a[href]').forEach((a) => {
        const href = a.getAttribute('href') ?? ''
        if (!href) return
        const existing = a.getAttribute('title')
        a.setAttribute('title', existing ? `${existing} — ${href}` : href)
        if (href.toLowerCase().startsWith('cid:')) {
          const cid = href.slice(4).trim().replace(/^<|>$/g, '')
          a.setAttribute('data-nimbus-cid', cid)
          // Neutralise default `cid:` navigation; our click handler takes over.
          a.setAttribute('href', '#')
        } else {
          // External links open in the system browser via open_url command.
          a.setAttribute('target', '_blank')
          a.setAttribute('rel', 'noopener noreferrer')
        }
      })

      // Block remote images unless the user has opted in for this message/sender
      let hadBlocked = false
      if (!showImages) {
        doc.querySelectorAll('img').forEach((img) => {
          const src = img.getAttribute('src') ?? ''
          if (src && !src.toLowerCase().startsWith('data:') && !src.toLowerCase().startsWith('cid:')) {
            hadBlocked = true
            img.setAttribute('data-nimbus-blocked-src', src)
            img.setAttribute('src', BLOCKED_IMG_PLACEHOLDER)
            img.removeAttribute('srcset')
            const alt = img.getAttribute('alt') ?? ''
            if (!alt) img.setAttribute('alt', '(blocked image)')
            img.setAttribute('title', 'Remote image blocked — click "Show images" to load')
          }
        })
      }

      return { html: doc.body.innerHTML, hadBlocked }
    } catch (e) {
      console.warn('processEmailHtml failed:', e)
      return { html: '', hadBlocked: false }
    }
  }

  // Recompute whenever the email body, per-message toggle, or trust state changes.
  let processedHtml = $derived.by(() => {
    if (email?.body_html) {
      return processEmailHtml(
        email.body_html,
        showImagesForMessage || trustedSender || autoLoadRemoteImages,
      )
    }
    // Plain-text-only message — synthesize a minimal HTML
    // wrapper so the same DOMPurify → auto-link → URLhaus
    // check → click-handler pipeline applies.  Without this
    // path, plain-text URLs would render unchecked and clicks
    // would bypass the unsafe-link confirm modal entirely
    // (#165 follow-up).  We HTML-escape first so any literal
    // `<` / `&` in the user's body stays as text rather than
    // being interpreted as markup.
    if (email?.body_text) {
      const wrapped = `<pre style="white-space: pre-wrap; font-family: inherit; margin: 0;">${escapeHtmlForPre(email.body_text)}</pre>`
      return processEmailHtml(
        wrapped,
        showImagesForMessage || trustedSender || autoLoadRemoteImages,
      )
    }
    return { html: '', hadBlocked: false }
  })

  /** Minimal HTML escape for the plain-text → HTML wrapper.
   *  We don't run user-supplied HTML through this — the body
   *  goes straight to DOMPurify after wrapping — but we still
   *  need to escape `<`, `>`, `&`, `"` so a plain-text body
   *  containing the literal sequence "<script>" doesn't get
   *  interpreted as markup before DOMPurify can sanitise it. */
  function escapeHtmlForPre(text: string): string {
    return text
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
  }

  // ── URLhaus link safety (#165) ───────────────────────────────────────
  //
  // Two-pass render.  Pass 1 (`processedHtml` above) is synchronous and
  // gives us the sanitised HTML immediately.  Pass 2 walks that HTML for
  // <a href> nodes, batches them into one `check_urls` IPC, and produces
  // an annotated HTML string with green / red pills inserted next to
  // each link.  Until pass 2 completes the user sees the sanitised body
  // *without* pills — never with a flash of the wrong colour.
  //
  // Verdicts are cached per message-id so re-opening the same message
  // doesn't re-run the lookup; the cache is cleared whenever `email`
  // changes to a different id.

  interface LinkVerdict {
    url: string
    /** `'safe'` | `'unsafe'` | `'off'` (the master toggle is off). */
    verdict: 'safe' | 'unsafe' | 'off'
    threat: string | null
    tags: string | null
    exact: boolean
  }
  let linkVerdicts = $state<Record<string, LinkVerdict>>({})
  let lastCheckedEmailId = $state<string | null>(null)

  /** Walk the processed HTML, harvest every distinct http(s) URL,
   *  and ask the backend for a verdict per URL.  Skips when the
   *  master toggle is off (the IPC short-circuits but we save a
   *  round-trip anyway). */
  $effect(() => {
    if (!email || !processedHtml.html) {
      linkVerdicts = {}
      lastCheckedEmailId = null
      return
    }
    if (!linkCheckEnabled) {
      linkVerdicts = {}
      return
    }
    if (lastCheckedEmailId === email.id) return
    const urls = extractCheckableUrls(processedHtml.html)
    if (urls.length === 0) {
      linkVerdicts = {}
      lastCheckedEmailId = email.id
      return
    }
    const expectedId = email.id
    void invoke<LinkVerdict[]>('check_urls', { urls })
      .then((rows) => {
        // Drop the response if the user moved on to a different
        // message before it landed — annotating a stale email
        // would paint pills on the wrong body.
        if (email?.id !== expectedId) return
        const map: Record<string, LinkVerdict> = {}
        for (const r of rows) map[r.url] = r
        linkVerdicts = map
        lastCheckedEmailId = expectedId
      })
      .catch((e) => {
        console.warn('check_urls failed', e)
      })
  })

  function extractCheckableUrls(html: string): string[] {
    const seen = new Set<string>()
    const out: string[] = []
    const doc = new DOMParser().parseFromString(html, 'text/html')
    doc.querySelectorAll('a[href]').forEach((a) => {
      const href = (a.getAttribute('href') ?? '').trim()
      const lower = href.toLowerCase()
      if (!lower.startsWith('http://') && !lower.startsWith('https://')) return
      if (seen.has(href)) return
      seen.add(href)
      out.push(href)
    })
    return out
  }

  /** Inject pill spans next to each <a href> based on the verdict
   *  map.  When the master toggle is off (or no verdicts have
   *  arrived yet) returns the input verbatim — no pills. */
  function annotateLinkPills(
    html: string,
    verdicts: Record<string, LinkVerdict>,
  ): string {
    if (Object.keys(verdicts).length === 0) return html
    try {
      const doc = new DOMParser().parseFromString(html, 'text/html')
      doc.querySelectorAll('a[href]').forEach((a) => {
        const href = (a.getAttribute('href') ?? '').trim()
        const v = verdicts[href]
        if (!v || v.verdict === 'off') return
        // Style is intentionally inline so it survives a future
        // Tailwind purge of class names that don't appear in any
        // .svelte file directly.  Pills sit immediately before
        // the link, separated by a thin no-break space so they
        // visually attach to the URL they describe.
        const pill = doc.createElement('span')
        pill.setAttribute('data-nimbus-link-pill', v.verdict)
        if (v.verdict === 'unsafe') {
          pill.style.cssText =
            'display:inline-block;font-size:0.7rem;font-weight:600;' +
            'padding:0.1rem 0.4rem;margin-right:0.25rem;border-radius:9999px;' +
            'background:#dc2626;color:#fff;vertical-align:middle;'
          pill.textContent = 'Unsafe'
          if (v.threat) {
            pill.title = v.exact
              ? `URLhaus flagged this URL — threat: ${v.threat}`
              : `URLhaus has flagged other URLs on this domain — threat: ${v.threat}`
          }
          // Mark the anchor so the click handler knows to
          // intercept and show the confirm modal.
          a.setAttribute('data-nimbus-unsafe-link', '1')
          if (v.threat) a.setAttribute('data-nimbus-threat', v.threat)
          if (v.exact) a.setAttribute('data-nimbus-link-exact', '1')
        } else {
          // Safe pill stays understated — a green dot pill so it
          // doesn't draw the eye away from the actual content.
          pill.style.cssText =
            'display:inline-block;font-size:0.7rem;font-weight:600;' +
            'padding:0.1rem 0.4rem;margin-right:0.25rem;border-radius:9999px;' +
            'background:#16a34a;color:#fff;vertical-align:middle;'
          pill.textContent = 'Safe'
          pill.title = 'No known threat indicators on URLhaus'
        }
        a.parentNode?.insertBefore(pill, a)
      })
      return doc.body.innerHTML
    } catch (e) {
      console.warn('annotateLinkPills failed', e)
      return html
    }
  }

  let annotatedHtml = $derived(
    !linkCheckEnabled || Object.keys(linkVerdicts).length === 0
      ? processedHtml.html
      : annotateLinkPills(processedHtml.html, linkVerdicts),
  )

  /** State for the "Unsafe link clicked" confirm modal.  When
   *  non-null, MailView paints the modal over the reading pane
   *  with two actions: Delete mail (move to Trash) and Open link
   *  anyway.  Esc / outside-click cancel. */
  let unsafeLinkPrompt = $state<
    { url: string; threat: string | null; exact: boolean } | null
  >(null)

  async function onUnsafeLinkOpenAnyway() {
    if (!unsafeLinkPrompt) return
    const url = unsafeLinkPrompt.url
    unsafeLinkPrompt = null
    try {
      await invoke('open_url', { url })
    } catch (e) {
      console.warn('open_url failed', e)
    }
  }
  async function onUnsafeLinkDeleteMail() {
    unsafeLinkPrompt = null
    if (!email) return
    // Soft delete via the existing toolbar path — moves the
    // message to Trash so a misclick is recoverable.
    try {
      await deleteMessage()
    } catch (e) {
      console.warn('delete after unsafe-link prompt failed', e)
    }
  }

  // ── Click handling for the inline HTML body div ───────────────────────
  //
  // cid: links open the matching attachment (same as before).
  // External http/https links are routed through the `open_url` Tauri
  // command so they open in the user's default system browser instead of
  // navigating inside the app's webview.

  function onBodyClick(e: MouseEvent) {
    const target = e.target as HTMLElement | null
    if (!target) return

    // Tiptap-rendered attachment refs from Nimbus (#93).
    // Two on-the-wire shapes float around:
    //   - new: <span data-attachment-ref data-cid=... data-filename=...>
    //   - legacy: <a href="cid:..." data-attachment-ref>
    // Plus an intermediate where DOMPurify's cid: handler has
    // already moved the cid into `data-nimbus-cid`.  We resolve
    // through every channel so a click works regardless of the
    // sending client's age or what survived the round-trip.
    const refEl = target.closest('[data-attachment-ref]') as HTMLElement | null
    if (refEl) {
      e.preventDefault()
      e.stopPropagation()
      if (!email) return
      // CID resolution: explicit data-cid → data-nimbus-cid
      // (set by processEmailHtml on legacy anchors) → href.
      let cidAttr = (refEl.getAttribute('data-cid') ?? '').trim()
      if (!cidAttr) cidAttr = (refEl.getAttribute('data-nimbus-cid') ?? '').trim()
      if (!cidAttr) {
        const href = (refEl.getAttribute('href') ?? '').trim()
        if (href.toLowerCase().startsWith('cid:')) {
          cidAttr = href.slice(4).replace(/^<|>$/g, '')
        }
      }
      const cidLower = cidAttr.toLowerCase()
      // Filename resolution: explicit data-filename →
      // data-label → the visible text after the leading badge
      // letters, as a last resort.
      let fnAttr = (
        refEl.getAttribute('data-filename') ?? refEl.getAttribute('data-label') ?? ''
      ).trim()
      if (!fnAttr) {
        fnAttr = (refEl.textContent ?? '')
          .trim()
          .replace(/^[A-Z]{2,4}\s+/, '')
      }
      const fnLower = fnAttr.toLowerCase()
      const att = email.attachments.find((a) => {
        if (cidLower && a.content_id && a.content_id.toLowerCase() === cidLower) return true
        if (fnLower && a.filename.toLowerCase() === fnLower) return true
        return false
      })
      if (att) void attachmentClicked(att)
      else
        console.warn(
          `MailView: attachment-ref click had no match (cid=${cidLower}, filename=${fnLower})`,
        )
      return
    }

    const anchor = target.closest('a') as HTMLAnchorElement | null
    if (!anchor) return

    const cid = anchor.getAttribute('data-nimbus-cid')
    if (cid) {
      e.preventDefault()
      e.stopPropagation()
      if (!email) return
      const att = email.attachments.find(
        (a) => a.content_id != null && a.content_id.toLowerCase() === cid.toLowerCase(),
      )
      if (!att) {
        console.warn(`MailView: cid:${cid} clicked but no matching attachment`)
        return
      }
      void attachmentClicked(att)
      return
    }

    const href = anchor.getAttribute('href') ?? ''
    if (!href || href === '#' || href.startsWith('javascript:')) return
    // mailto: → open Compose pre-filled rather than handing the
    // URL to the OS (which would launch the default mail handler,
    // unhelpful when Nimbus *is* the user's mail client).  RFC 6068
    // allows `mailto:to?subject=...&cc=...&bcc=...&body=...`, with
    // multiple addresses comma-separated and percent-encoded.
    if (href.toLowerCase().startsWith('mailto:')) {
      e.preventDefault()
      e.stopPropagation()
      const init = parseMailtoUrl(href)
      onmailto?.(init)
      return
    }
    // #165 — URLhaus-flagged links go through a confirm modal
    // instead of opening straight to the system browser.  The
    // anchor is tagged with `data-nimbus-unsafe-link` by
    // `annotateLinkPills` only when the verdict came back
    // 'unsafe' (and the master toggle is on), so a missing
    // attribute is the safe / off path that keeps the original
    // open-in-browser behaviour.
    if (anchor.hasAttribute('data-nimbus-unsafe-link')) {
      e.preventDefault()
      e.stopPropagation()
      unsafeLinkPrompt = {
        url: href,
        threat: anchor.getAttribute('data-nimbus-threat'),
        exact: anchor.hasAttribute('data-nimbus-link-exact'),
      }
      return
    }
    e.preventDefault()
    void invoke('open_url', { url: href })
  }

  /** Parse a `mailto:` URL into ComposeInitial-shaped fields per
   *  RFC 6068.  Tolerant: missing pieces just stay undefined so
   *  the caller's defaults take over. */
  function parseMailtoUrl(raw: string): {
    to?: string
    cc?: string
    bcc?: string
    subject?: string
    body?: string
  } {
    const stripped = raw.replace(/^mailto:/i, '')
    const qIdx = stripped.indexOf('?')
    const recipientsPart = qIdx === -1 ? stripped : stripped.slice(0, qIdx)
    const queryPart = qIdx === -1 ? '' : stripped.slice(qIdx + 1)
    const decode = (s: string) => {
      try {
        return decodeURIComponent(s.replace(/\+/g, '%20'))
      } catch {
        return s
      }
    }
    const out: { to?: string; cc?: string; bcc?: string; subject?: string; body?: string } = {}
    if (recipientsPart) out.to = decode(recipientsPart)
    if (!queryPart) return out
    for (const pair of queryPart.split('&')) {
      if (!pair) continue
      const eq = pair.indexOf('=')
      const key = (eq === -1 ? pair : pair.slice(0, eq)).toLowerCase()
      const val = eq === -1 ? '' : decode(pair.slice(eq + 1))
      switch (key) {
        case 'to':
          out.to = out.to ? `${out.to}, ${val}` : val
          break
        case 'cc':
          out.cc = out.cc ? `${out.cc}, ${val}` : val
          break
        case 'bcc':
          out.bcc = out.bcc ? `${out.bcc}, ${val}` : val
          break
        case 'subject':
          out.subject = val
          break
        case 'body':
          out.body = val
          break
      }
    }
    return out
  }

  /** Single dispatch point for any user-driven attachment open
   *  request (cid:-anchor clicks, primary chip-button clicks).
   *  Type detection + per-tier behaviour live in
   *  `./attachmentOpen.ts`; this wrapper just resolves the
   *  bytes lazily via `download_email_attachment`.  Default for
   *  non-Office/non-PDF/non-Markdown types is now "Open in
   *  Desktop App" (#162) instead of falling through to
   *  Download — Save-to-disk stays available in the dropdown. */
  async function attachmentClicked(att: EmailAttachment) {
    if (!email || uid == null) return
    const acc = email.account_id
    const fld = email.folder
    const u = uid
    setBusy(att.part_id, true)
    try {
      await openAttachment(att, () =>
        invoke<number[]>('download_email_attachment', {
          accountId: acc,
          folder: fld,
          uid: u,
          partId: att.part_id,
        }),
      )
    } catch (e) {
      error = formatError(e) || 'Failed to open attachment'
    } finally {
      setBusy(att.part_id, false)
    }
  }

  // ---------------------------------------------------------------------
  // Attachments — download to disk or save into a Nextcloud folder.
  // ---------------------------------------------------------------------

  // Per-attachment in-flight flags, keyed by part_id. Lets us show a
  // spinner / disable just the row the user clicked instead of locking
  // the whole list.
  let busyParts = $state<Set<number>>(new Set())
  // Set when the user clicks "Save to Nextcloud" on an attachment —
  // mounts the file picker in folder-pick mode. Once a folder is picked
  // we upload the bytes there.
  let savingAttachment = $state<EmailAttachment | null>(null)

  function setBusy(partId: number, busy: boolean) {
    const next = new Set(busyParts)
    if (busy) next.add(partId)
    else next.delete(partId)
    busyParts = next
  }

  function formatAttSize(bytes: number | null): string {
    if (bytes == null) return ''
    if (bytes < 1024) return `${bytes} B`
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`
    return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`
  }

  /** Always returns `null` now — every attachment falls through
   *  to `FileTypeIcon`, which knows how to draw a typed badge
   *  (PDF / DOC / XLS / …) when it recognises the format, and a
   *  plain document silhouette as the universal fallback for
   *  unrecognised content-types and extensionless filenames.
   *  The function is kept as a single seam so future per-type
   *  emoji can be reintroduced without re-threading the chip
   *  render below. */
  function attachmentEmoji(_att: EmailAttachment): string | null {
    return null
  }

  /**
   * Download an attachment to a user-chosen location on disk.
   *
   * Flow:
   * 1. Open a native "Save As" dialog (prefilled with the attachment
   *    filename) via `@tauri-apps/plugin-dialog`.
   * 2. If the user cancels, bail without fetching bytes — no point
   *    pulling a multi-MB attachment just to throw it away.
   * 3. Otherwise re-fetch the bytes through `download_email_attachment`
   *    and write them to the chosen path via `save_bytes_to_path`.
   *
   * Why not a synthetic `<a download>` like the earlier version? The
   * WebView 2 / WebKit implementations that Tauri sits on top of don't
   * reliably prompt for a save location — the file either lands in the
   * system Downloads folder or the download fails silently. The native
   * dialog is the only consistent way to let the user pick a path.
   */
  async function downloadAttachment(att: EmailAttachment) {
    if (!email) return
    // Use the `uid` prop directly — `email.id` is a composite string
    // like `{account}-{folder}-{uid}` and parseInt'ing it gives NaN,
    // which serializes to null and fails Tauri's u32 validation.
    if (uid == null) return

    // Ask for a save location first. If the user hits Cancel, `save`
    // resolves to `null` and we stop — no network, no write, no noise.
    let chosenPath: string | null = null
    try {
      chosenPath = await save({
        defaultPath: att.filename,
        title: 'Save attachment',
      })
    } catch (e) {
      error = formatError(e) || 'Failed to open save dialog'
      return
    }
    if (!chosenPath) return

    setBusy(att.part_id, true)
    try {
      const bytes = await invoke<number[]>('download_email_attachment', {
        accountId: email.account_id,
        folder: email.folder,
        uid,
        partId: att.part_id,
      })
      await invoke('save_bytes_to_path', { path: chosenPath, data: bytes })
    } catch (e) {
      error = formatError(e) || 'Failed to download attachment'
    } finally {
      setBusy(att.part_id, false)
    }
  }

  /**
   * Open the Nextcloud picker in folder-pick mode. The picker calls
   * `onSavePicked` with the chosen folder; we then download the
   * attachment bytes and PUT them into that folder.
   */
  function startSaveToNextcloud(att: EmailAttachment) {
    savingAttachment = att
  }

  /** Pull bytes for the attachment and hand them to the backend's
   *  `print_attachment`, which writes the file to OS temp and
   *  opens it with the user's default app for that file type
   *  (Word for .docx, Edge / Acrobat for .pdf, Photos for images,
   *  Notepad for text, etc.). The user then hits Ctrl/Cmd-P from
   *  inside that app to get the system printer-chooser dialog. */
  async function printAttachment(att: EmailAttachment) {
    if (!email || uid == null) return
    setBusy(att.part_id, true)
    try {
      const bytes = await invoke<number[]>('download_email_attachment', {
        accountId: email.account_id,
        folder: email.folder,
        uid,
        partId: att.part_id,
      })
      await invoke('print_attachment', {
        fileName: att.filename,
        bytes,
      })
    } catch (e) {
      error = formatError(e) || 'Failed to print attachment'
    } finally {
      setBusy(att.part_id, false)
    }
  }

  /** Copy the attachment filename to the clipboard. Useful when the
   *  user wants to paste it into another app (e.g. as a reference
   *  in a Talk message) without saving the file first. */
  async function copyFilename(att: EmailAttachment) {
    try {
      await navigator.clipboard.writeText(att.filename)
    } catch (e) {
      console.warn('clipboard write failed', e)
    }
  }

  // ── Per-attachment action menu (chevron dropdown) ──
  // One menu open at a time, keyed by `part_id`. `null` = closed.
  // Anchor + position are captured at click time so the popup floats
  // next to the row that owns it without needing a portal.
  let openMenuFor = $state<number | null>(null)

  function toggleMenu(att: EmailAttachment) {
    openMenuFor = openMenuFor === att.part_id ? null : att.part_id
  }
  function closeMenu() {
    openMenuFor = null
  }

  /** Click handler that runs an action and closes the menu in one
   *  go. `void`-wraps async handlers so the inline onclick stays
   *  synchronous (Svelte warns otherwise). */
  function runAndClose(fn: () => void | Promise<void>) {
    return () => {
      closeMenu()
      void fn()
    }
  }

  async function onSavePicked(ncId: string, folderPath: string) {
    const att = savingAttachment
    savingAttachment = null
    if (!email || !att) return
    setBusy(att.part_id, true)
    try {
      // Use the `uid` prop directly — `email.id` is a composite string
      // like `{account}-{folder}-{uid}` and parseInt'ing it gives NaN,
      // which serializes to null and fails Tauri's u32 validation.
      if (uid == null) return
      const bytes = await invoke<number[]>('download_email_attachment', {
        accountId: email.account_id,
        folder: email.folder,
        uid,
        partId: att.part_id,
      })
      // Join the folder path with the filename, avoiding double slashes
      // when folderPath is just '/'.
      const base = folderPath.endsWith('/') ? folderPath : `${folderPath}/`
      const target = `${base}${att.filename}`
      await invoke('upload_to_nextcloud', {
        ncId,
        path: target,
        data: bytes,
        contentType: att.content_type || null,
      })
    } catch (e) {
      error = formatError(e) || 'Failed to save to Nextcloud'
    } finally {
      setBusy(att.part_id, false)
    }
  }

  // ---------------------------------------------------------------------
  // Archive / Delete / Move-to-folder — top-bar actions that remove the
  // current message from the visible folder.  All three are optimistic
  // (#174): notify the parent to auto-advance immediately, then run
  // the IMAP command in the background.  IMAP errors land in the same
  // `error` banner the load path uses, and the backend's tombstone /
  // un-tombstone lifecycle restores the row in the list on failure.
  // ---------------------------------------------------------------------
  // Move-to-folder picker (#89).  The picker itself is a separate
  // modal component — `MoveFolderPicker` — that fetches folders and
  // renders them with the same icon/order conventions as the
  // sidebar.  We just hold a flag for "is the picker mounted" and
  // an `onpicked` handler that fires the move.
  let moveMenuOpen = $state(false)

  // Optimistic-action helper (#174).  Notify the parent FIRST so
  // the auto-advance + MailList tombstone-driven row removal both
  // run before the IMAP roundtrip.  If the IMAP call fails the
  // backend's `clear_message_pending` un-tombstones the cache row
  // so the list pull restores it; we surface the error message
  // here so the user knows the action didn't actually take.
  async function moveToFolder(destFolder: string) {
    if (!email || uid == null) return
    if (destFolder === email.folder) return // move-to-self is a noop
    const removedUid = uid
    const acc = email.account_id
    const fld = email.folder
    onmessageremoved?.(removedUid)
    try {
      await invoke('move_message', {
        accountId: acc,
        folder: fld,
        uid: removedUid,
        destFolder,
      })
    } catch (e) {
      error = formatError(e) || 'Failed to move'
    }
  }

  async function archiveMessage() {
    if (!email || uid == null) return
    const removedUid = uid
    const acc = email.account_id
    const fld = email.folder
    // Whole-thread archive (#289 follow-up): when the parent's
    // selection-aware derivation has handed us a member-UID list,
    // the open message is the head of a multi-member thread and a
    // single archive click should sweep every member.  We fire
    // `onmessageremoved` for each UID FIRST (so MailList's
    // bound-envelope splice and the auto-advance logic both run
    // against the still-populated list), then dispatch the batched
    // backend call.  Falls back to the single-UID path otherwise
    // — no need to invoke the batch IPC for a single message.
    const members =
      threadMemberUids && threadMemberUids.length > 1
        ? threadMemberUids
        : null
    if (members) {
      for (const u of members) onmessageremoved?.(u)
      try {
        await invoke('archive_messages', {
          accountId: acc,
          folder: fld,
          uids: members,
        })
      } catch (e) {
        error = formatError(e) || 'Failed to archive'
      }
      return
    }
    onmessageremoved?.(removedUid)
    try {
      await invoke('archive_message', {
        accountId: acc,
        folder: fld,
        uid: removedUid,
      })
    } catch (e) {
      error = formatError(e) || 'Failed to archive'
    }
  }

  async function deleteMessage() {
    if (!email || uid == null) return
    // No confirm dialog yet — matches the "click = commit" shape of
    // the rest of the toolbar. A Trash-folder intermediate (and
    // undo) can come later; for now Delete is outright expunge.
    const removedUid = uid
    const acc = email.account_id
    const fld = email.folder
    onmessageremoved?.(removedUid)
    try {
      await invoke('delete_message', {
        accountId: acc,
        folder: fld,
        uid: removedUid,
      })
    } catch (e) {
      error = formatError(e) || 'Failed to delete'
    }
  }
</script>

<main class="flex-1 flex flex-col overflow-hidden">
  {#if uid == null}
    <div class="flex-1 flex items-center justify-center text-surface-500">
      Select a message to read.
    </div>
  {:else if loading}
    <div class="flex-1 flex items-center justify-center text-surface-500">Loading message…</div>
  {:else if error}
    <div class="p-6 text-sm text-red-500">{error}</div>
  {:else if email}
    <!-- Email header -->
    <div class="p-6 border-b border-surface-200 dark:border-surface-700">
      <div class="flex items-start justify-between mb-2 gap-4">
        <h2 class="text-xl font-semibold">{email.subject || '(no subject)'}</h2>
        <div class="flex items-center gap-3 shrink-0">
          <span class="text-sm text-surface-500">{formatFullDate(email.date)}</span>
        </div>
      </div>
      <div class="flex items-center gap-2 text-sm text-surface-600 dark:text-surface-400">
        <span class="font-medium">{email.from || '(unknown sender)'}</span>
      </div>
      {#if email.to.length > 0}
        <div class="text-xs text-surface-500 mt-1">
          To: {email.to.join(', ')}
        </div>
      {/if}
      {#if email.cc.length > 0}
        <div class="text-xs text-surface-500">
          Cc: {email.cc.join(', ')}
        </div>
      {/if}
    </div>

    <!-- Action bar. The Drafts folder shows an "Edit" action instead
         of the reply/forward/mark-read cluster — a draft is the user's
         own unfinished work, so re-opening it in Compose is the only
         gesture that makes sense. -->
    <div class="flex items-center gap-2 px-6 py-2 border-b border-surface-200 dark:border-surface-700 text-sm">
      <!-- Toolbar action buttons (#179): icon-only with hover
           tooltips.  Labels live in `title` + `aria-label` so the
           strip stays compact and the visual rhythm is uniform.
           Edit-draft keeps its label because it's the *only*
           affordance in the Drafts variant — losing the word
           there would leave the toolbar with a single mystery
           pencil. -->
      {#if isDraftsFolder}
        <button
          class="btn btn-sm preset-filled-primary-500 inline-flex items-center gap-1.5"
          onclick={() => email && oneditdraft?.(email)}
          title="Open this draft in Compose to keep editing"
        ><Icon name="compose" size={16} /> Edit draft</button>
      {:else}
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
          onclick={() => email && uid != null && onreply?.({ ...email, uid })}
          title="Reply"
          aria-label="Reply"
        ><Icon name="reply" size={16} /></button>
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
          onclick={() => email && uid != null && onreplyall?.({ ...email, uid })}
          title="Reply to everyone"
          aria-label="Reply to everyone"
        ><Icon name="reply-all" size={16} /></button>
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
          onclick={() => email && uid != null && onforward?.({ ...email, uid })}
          title="Forward"
          aria-label="Forward"
        ><Icon name="forward" size={16} /></button>
        {#if onrespondwithmeeting}
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
            onclick={() => email && uid != null && onrespondwithmeeting?.({ ...email, uid })}
            title="Create a calendar event with a Talk link and the thread's participants as attendees"
            aria-label="Respond with meeting"
          ><Icon name="respond-with-meeting" size={16} /></button>
        {/if}
        {#if onsavenote}
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
            onclick={() => email && onsavenote?.(email)}
            title="Save this email as a Nextcloud note"
            aria-label="Save as note"
          ><Icon name="notes" size={16} /></button>
        {/if}
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
          onclick={toggleRead}
          title={email.is_read ? 'Mark this message as unread' : 'Mark this message as read'}
          aria-label={email.is_read ? 'Mark as unread' : 'Mark as read'}
        ><Icon name={email.is_read ? 'unread' : 'read'} size={16} /></button>
      {/if}
      <div class="flex-1"></div>
      {#if !inStandaloneWindow && email && uid != null}
        <!-- Pop the open mail into its own focused window (#104).
             Hidden when we're already *in* the standalone window —
             a click there would just spawn another identical
             window, which is never what you want. -->
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
          onclick={() => email && uid != null && openMailInStandaloneWindow(email.account_id, email.folder, uid)}
          title="Open this mail in a separate window"
          aria-label="Open in window"
        ><Icon name="full-screen" size={16} /></button>
      {/if}
      {#if email.body_html}
        <!-- Per-message background toggle — flips the white-canvas
             default just for the open mail.  Icon-only: `sun` for
             "switch to white" (bright canvas), `design-palette` for
             "switch to the app's theme" (whatever palette the user
             picked).  Title carries the action so hover tooltips
             still spell it out. -->
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
          onclick={() => (whiteBackgroundOverride = !effectiveWhiteBackground)}
          title={effectiveWhiteBackground
            ? "Switch this mail to the app's theme background"
            : 'Switch this mail to a white background'}
          aria-label={effectiveWhiteBackground ? 'Use mail theme' : 'White background'}
        ><Icon name={effectiveWhiteBackground ? 'design-palette' : 'sun'} size={16} /></button>
      {/if}
      <!-- Move to folder (#89) — single button that opens the
           `MoveFolderPicker` modal.  Picker presents folders with
           the same icons + ordering the sidebar uses, plus an
           inline filter for accounts with lots of folders. -->
      <button
        class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
        onclick={() => (moveMenuOpen = true)}
        title="Move this message to a different folder"
        aria-label="Move to folder"
      ><Icon name="move-to-folder" size={16} /></button>
      <button
        class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
        onclick={archiveMessage}
        title="Move this message to the Archive folder"
        aria-label="Archive"
      ><Icon name="archive" size={16} /></button>
      <button
        class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
        onclick={deleteMessage}
        title="Move this message to Trash (permanently deletes if already in Trash or if the account has no Trash folder)"
        aria-label="Delete"
      ><Icon name="trash" size={16} /></button>
    </div>

    <!-- Calendar invite (#58 / iMIP).  Mounted above the
         attachment list so the user reaches for Accept / Decline
         before scanning the rest of the message body. -->
    {#if invite && !isSentFolder}
      <div class="px-6 pt-3">
        <CalendarInviteCard
          invite={invite}
          onresponded={() => {
            // The replied invite stays visible (the response chip
            // tells the user what they sent) — no re-fetch needed
            // since the RSVP doesn't change the inbound mail.
          }}
        />
      </div>
    {/if}

    <!-- Attachments — only renders when the message actually has any. -->
    {#if email.attachments.length > 0}
      <div class="px-6 py-3 border-b border-surface-200 dark:border-surface-700">
        <div class="text-xs font-semibold text-surface-500 mb-2">
          {email.attachments.length} attachment{email.attachments.length === 1 ? '' : 's'}
        </div>
        <ul class="flex flex-wrap gap-2">
          {#each email.attachments as att (att.part_id)}
            {@const busy = busyParts.has(att.part_id)}
            {@const isOffice = isOfficeAttachment(att)}
            {@const isPdf = isPdfAttachment(att)}
            {@const isMarkdown = isMarkdownAttachment(att)}
            {@const menuOpen = openMenuFor === att.part_id}
            {@const emoji = attachmentEmoji(att)}
            <li class="relative flex items-center gap-2 pl-3 pr-1 py-1.5 rounded-md bg-surface-100 dark:bg-surface-800 text-sm">
              {#if emoji}
                <span class="text-base">{emoji}</span>
              {:else}
                <!-- Bytes are lazy-fetched only when the chip
                     mounts and only for sub-threshold sizes —
                     a 100 MiB MOV shouldn't trigger an IPC just
                     to render a 36×36 cell.  Images cap at 4 MiB
                     (typical phone photo); videos cap at 16 MiB
                     (most short clips that actually fit on a
                     mail server). -->
                {@const isVid = (att.content_type || '').startsWith('video/')}
                {@const sizeCap = isVid ? 16 * 1024 * 1024 : 4 * 1024 * 1024}
                {@const tooLarge = att.size != null && att.size > sizeCap}
                <AttachmentThumb
                  contentType={att.content_type}
                  filename={att.filename}
                  cacheKey={`${email!.account_id}::${email!.folder}::${uid}::${att.part_id}`}
                  persistTo={{
                    accountId: email!.account_id,
                    folder: email!.folder,
                    uid: uid!,
                    partId: att.part_id,
                  }}
                  bytesProvider={tooLarge
                    ? undefined
                    : () =>
                        invoke<number[]>('download_email_attachment', {
                          accountId: email!.account_id,
                          folder: email!.folder,
                          uid: uid!,
                          partId: att.part_id,
                        })}
                  class="w-9 h-9"
                />
              {/if}
              <span class="font-medium truncate max-w-60" title={att.filename}>{att.filename}</span>
              {#if att.size != null}
                <span class="text-xs text-surface-500">{formatAttSize(att.size)}</span>
              {/if}

              <!-- Primary action — picks the most natural open
                   verb for the attachment type. Same as a click on
                   the chip itself; the dropdown to the right
                   exposes everything else (Print, Download, Save
                   to NC, Copy filename). The standard
                   "click = open, ▾ = more" pattern.
                   #162: types we don't have a webview viewer for
                   default to "Open" (open-in-desktop-app), not
                   Download — Save-to-disk stays available below. -->
              {#if isOffice}
                <button
                  class="btn btn-sm preset-filled-primary-500 text-xs"
                  disabled={busy}
                  onclick={() => attachmentClicked(att)}
                  title="Open in Nextcloud Office (Collabora)"
                >
                  {#if busy}
                    …
                  {:else}
                    <Icon name="open-in-browser" size={12} class="inline-block align-text-bottom mr-1" />Open in Office
                  {/if}
                </button>
              {:else if isPdf}
                <button
                  class="btn btn-sm preset-filled-primary-500 text-xs"
                  disabled={busy}
                  onclick={() => attachmentClicked(att)}
                  title="Open in Nextcloud's built-in PDF viewer"
                >
                  {#if busy}
                    …
                  {:else}
                    <Icon name="open-in-browser" size={12} class="inline-block align-text-bottom mr-1" />Open PDF
                  {/if}
                </button>
              {:else if isMarkdown}
                <button
                  class="btn btn-sm preset-filled-primary-500 text-xs"
                  disabled={busy}
                  onclick={() => attachmentClicked(att)}
                  title="Render the markdown in a read-only viewer window"
                >
                  {#if busy}
                    …
                  {:else}
                    <Icon name="open-in-browser" size={12} class="inline-block align-text-bottom mr-1" />Open Markdown
                  {/if}
                </button>
              {:else}
                <button
                  class="btn btn-sm preset-filled-primary-500 text-xs inline-flex items-center gap-1.5"
                  disabled={busy}
                  onclick={() => attachmentClicked(att)}
                  title="Open in your default desktop app for this file type"
                >
                  {#if busy}
                    …
                  {:else}
                    <Icon name="open-on-desktop" size={12} /> Open
                  {/if}
                </button>
              {/if}

              <!-- Chevron toggle. Sits flush against the primary
                   button so they read as one pill with a split
                   click target. -->
              <button
                class="btn btn-sm preset-outlined-surface-500 text-xs px-2 inline-flex items-center"
                disabled={busy}
                aria-haspopup="menu"
                aria-expanded={menuOpen}
                aria-label="More attachment actions"
                onclick={() => toggleMenu(att)}
                title="More actions"
              ><Icon name="more" size={14} /></button>

              {#if menuOpen}
                <!-- Click-outside catcher. Sits behind the menu so
                     anywhere outside dismisses, but the menu itself
                     (z-50) stays above and receives clicks. -->
                <button
                  type="button"
                  class="fixed inset-0 z-40 cursor-default"
                  aria-label="Close menu"
                  onclick={closeMenu}
                ></button>
                <div
                  role="menu"
                  class="absolute right-0 top-full mt-1 z-50 min-w-52 rounded-md shadow-lg border border-surface-300 dark:border-surface-700 bg-surface-50 dark:bg-surface-900 py-1 text-sm"
                >
                  {#if isOffice}
                    <button
                      role="menuitem"
                      class="w-full text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 inline-flex items-center gap-1.5"
                      onclick={runAndClose(() => attachmentClicked(att))}
                    ><Icon name="open-in-browser" size={14} /> Open in Office</button>
                  {:else if isPdf}
                    <button
                      role="menuitem"
                      class="w-full text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 inline-flex items-center gap-2"
                      onclick={runAndClose(() => attachmentClicked(att))}
                    ><Icon name="open-in-browser" size={14} /> Open PDF</button>
                  {:else if isMarkdown}
                    <button
                      role="menuitem"
                      class="w-full text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 inline-flex items-center gap-2"
                      onclick={runAndClose(() => attachmentClicked(att))}
                    ><Icon name="open-in-browser" size={14} /> Open Markdown</button>
                  {/if}
                  <button
                    role="menuitem"
                    class="w-full text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 inline-flex items-center gap-2"
                    onclick={runAndClose(() => printAttachment(att))}
                    title="Open this attachment in its default desktop app (Ctrl/Cmd-P there to print)"
                  ><Icon name="open-on-desktop" size={14} /> Open in Desktop App</button>
                  <button
                    role="menuitem"
                    class="w-full text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 inline-flex items-center gap-2"
                    onclick={runAndClose(() => downloadAttachment(att))}
                  ><Icon name="download" size={14} /> Save to disk…</button>
                  <button
                    role="menuitem"
                    class="w-full text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 inline-flex items-center gap-2"
                    onclick={runAndClose(() => startSaveToNextcloud(att))}
                  ><Icon name="cloud" size={14} /> Save to Nextcloud…</button>
                  <div class="my-1 border-t border-surface-200 dark:border-surface-700"></div>
                  <button
                    role="menuitem"
                    class="w-full text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 inline-flex items-center gap-2"
                    onclick={runAndClose(() => copyFilename(att))}
                  ><Icon name="share-links" size={14} /> Copy filename</button>
                </div>
              {/if}
            </li>
          {/each}
        </ul>
      </div>
    {/if}

    <!-- Email body. Prefer HTML when present — multipart/alternative
         senders (GitHub, newsletters, almost everything modern) include
         a plain-text fallback for clients that can't render HTML, but
         the HTML is what carries the real formatting (layout, links,
         brand styles). DOMPurify + remote-image blocking make this
         safe; we fall back to plain text only when no HTML part exists. -->
    <div class="flex-1 overflow-y-auto">
      {#if processedHtml.html}
        <!-- Image-blocking banner — only visible when at least one remote
             image was replaced with a placeholder and the user hasn't opted
             in for this message or trusted this sender. -->
        {#if processedHtml.hadBlocked && !showImagesForMessage && !trustedSender}
          <div class="flex flex-wrap items-center gap-3 px-6 py-2 bg-amber-50 dark:bg-amber-900/20 border-b border-amber-200 dark:border-amber-700 text-sm text-amber-800 dark:text-amber-300">
            <span class="shrink-0 inline-flex items-center gap-2">
              <Icon name="shield-image-blocked" size={24} />
              Remote images are blocked.
            </span>
            <button
              class="btn btn-sm preset-outlined-surface-500"
              onclick={() => (showImagesForMessage = true)}
            >Show images</button>
            {#if email.from}
              <button
                class="btn btn-sm preset-outlined-surface-500"
                onclick={() => {
                  addTrustedSender(email!.from)
                  trustedSender = true
                }}
              >Always show from {getSenderAddress(email.from)}</button>
            {/if}
          </div>
        {/if}
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
        <div
          class="email-html-body text-sm leading-relaxed overflow-x-auto {effectiveWhiteBackground
            ? 'email-html-body--white'
            : 'email-html-body--native p-6'}"
          role="region"
          aria-label="Email body"
          onclick={onBodyClick}
        >
          {@html annotatedHtml}
        </div>
      {:else if email.body_text}
        <!-- This branch is now reachable only when the
             plain-text body fed through processEmailHtml above
             produced an empty result for some reason (e.g.
             DOMPurify nuked a particularly weird wrapper).
             Falls back to the raw text so the message still
             reads — at the cost of skipping the URLhaus check
             for that specific malformed case. -->
        <pre class="whitespace-pre-wrap font-sans text-sm p-6">{email.body_text}</pre>
      {:else}
        <p class="text-sm text-surface-500 p-6">(This message has no visible body.)</p>
      {/if}
    </div>
  {/if}

  {#if savingAttachment}
    <!--
      The picker takes the usual onpicked/onclose pair, but we don't use
      onpicked here — the picker is opened in folder-pick mode (via
      onpickfolder), which short-circuits the per-file selection flow
      entirely. The empty onpicked is just to satisfy the prop.
    -->
    <NextcloudFilePicker
      onpicked={() => {}}
      onpickfolder={onSavePicked}
      onclose={() => (savingAttachment = null)}
    />
  {/if}

  {#if moveMenuOpen && email}
    <MoveFolderPicker
      accountId={email.account_id}
      currentFolder={email.folder}
      onpicked={(name) => void moveToFolder(name)}
      onclose={() => (moveMenuOpen = false)}
    />
  {/if}

  <!-- #165 — confirm modal shown when the user clicks an
       URLhaus-flagged link.  Two primary actions, plus an
       implicit Cancel via Escape / outside-click.  Soft delete
       (move to Trash) so a misclick is recoverable. -->
  {#if unsafeLinkPrompt}
    <div
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
      role="dialog"
      aria-modal="true"
      tabindex="-1"
      onmousedown={(e) => {
        if (e.target === e.currentTarget) unsafeLinkPrompt = null
      }}
      onkeydown={(e) => {
        if (e.key === 'Escape') unsafeLinkPrompt = null
      }}
    >
      <div class="bg-surface-50 dark:bg-surface-900 rounded-lg shadow-xl w-md max-w-full mx-4 p-6 space-y-4">
        <div class="flex items-start gap-3">
          <span class="text-2xl" aria-hidden="true">⚠️</span>
          <div class="flex-1 min-w-0">
            <h3 class="text-base font-semibold">This link is on URLhaus</h3>
            <p class="text-sm text-surface-600 dark:text-surface-300 mt-1">
              {#if unsafeLinkPrompt.exact}
                The exact URL has been flagged as malicious.
              {:else}
                Other URLs on this domain have been flagged as malicious — this specific URL isn't on the list, but the domain has hosted malware before.
              {/if}
              {#if unsafeLinkPrompt.threat}
                <br>Threat: <code>{unsafeLinkPrompt.threat}</code>
              {/if}
            </p>
            <p class="text-xs text-surface-500 mt-2 break-all">
              <strong>URL:</strong> {unsafeLinkPrompt.url}
            </p>
          </div>
        </div>
        <div class="flex flex-wrap gap-2 justify-end">
          <button
            type="button"
            class="btn preset-outlined-surface-500"
            onclick={() => (unsafeLinkPrompt = null)}
          >Cancel</button>
          <button
            type="button"
            class="btn preset-outlined-error-500"
            onclick={() => void onUnsafeLinkDeleteMail()}
          >Delete mail</button>
          <button
            type="button"
            class="btn preset-filled-error-500"
            onclick={() => void onUnsafeLinkOpenAnyway()}
          >Open link anyway</button>
        </div>
      </div>
    </div>
  {/if}
</main>

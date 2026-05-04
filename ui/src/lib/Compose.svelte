<script lang="ts">
  /**
   * Compose — a modal for writing and sending an email.
   *
   * Handles new messages, replies, reply-all, and forwards. The parent
   * opens it with an optional `initial` prefill (used for reply / forward
   * to carry over To/Subject/quoted body).
   *
   * Body is plain text for now — a rich-text/Markdown editor is a later
   * enhancement. Attachments are read in the browser via FileReader so
   * we don't need the Tauri dialog plugin; the raw bytes are shipped to
   * the backend as a byte array.
   *
   * Drafts are saved to localStorage under a per-account key. That's
   * intentionally minimal — proper "Save to Drafts folder via IMAP APPEND"
   * is tracked separately.
   */

  import { convertFileSrc, invoke } from '@tauri-apps/api/core'
  import { untrack } from 'svelte'
  import { formatError } from './errors'
  import { extractManagedShares } from './managedShares'
  import {
    meetingInviteHtml,
    talkInviteHtml,
    type MeetingInvite,
  } from './inviteHtml'
  import RichTextEditor, {
    type EditorApi,
    type ContactSuggestion,
    type ExtraTab,
  } from './RichTextEditor.svelte'
  import AddressAutocomplete from './AddressAutocomplete.svelte'
  import NextcloudFilePicker, { type ShareLink } from './NextcloudFilePicker.svelte'
  import Icon from './Icon.svelte'
  import FileTypeIcon from './FileTypeIcon.svelte'
  import AttachmentThumb, { prewarm as prewarmAttachmentThumb } from './AttachmentThumb.svelte'
  import { openAttachment } from './attachmentOpen'
  import { mentionsAttachment } from './attachmentMentions'
  import { m } from '../paraglide/messages'
  import CreateTalkRoomModal, { type TalkRoom } from './CreateTalkRoomModal.svelte'
  import EventEditor, { type SavedEvent } from './EventEditor.svelte'
  import { openComposeInStandaloneWindow } from './standaloneComposeWindow'

  /** Slim Nextcloud account row — same shape `TalkView` / `Sidebar` use.
      We only need the id to pass to the Talk + Calendar commands. */
  interface NextcloudAccount {
    id: string
  }
  /** Mail account row for the From: picker. Mirrors the public fields
      of the Rust `Account` we actually consume here. */
  interface MailAccount {
    id: string
    display_name: string
    email: string
    signature?: string | null
    /** Optional human's full name for outbound From: (#115).
     *  When set, outgoing mail goes out as
     *  `"Person Name" <email>`; when null we fall back to the
     *  account's display_name. */
    person_name?: string | null
  }
  /** Slim calendar summary — matches the Rust `CalendarSummary` Tauri
      return shape. We pass the full list to `EventEditor` so the user
      can pick which calendar the event lands in. */
  interface CalendarSummary {
    id: string
    nextcloud_account_id: string
    display_name: string
    color: string | null
    last_synced_at: string | null
    /** Local visibility flag — hidden calendars are filtered out
     *  before the event-editor dropdown sees them so the "Add
     *  event" flow matches the CalendarView sidebar. */
    hidden?: boolean
  }

  interface Attachment {
    filename: string
    content_type: string
    data: number[]
    /** RFC 2392 Content-ID, assigned when the attachment is picked
     *  so the body HTML can reference it via `<a href="cid:…">`.
     *  The `/` attachment-picker shortcut in the editor inserts
     *  those links; recipients' mail clients that honour cid: get a
     *  clickable anchor, others see a plain-text link that falls
     *  through to the attachment tray below the message. */
    content_id?: string
  }

  export interface ComposeInitial {
    to?: string
    cc?: string
    bcc?: string
    subject?: string
    body?: string
    in_reply_to?: number | null
    /** Files to pre-attach. Used by `FilesView`'s "New mail with
        attachment" action so the user can pick files in the Files
        view and land in Compose with them already attached. */
    attachments?: Attachment[]
    /** Public Nextcloud share URLs to render into the body as a
        "Shared via Nextcloud" block. Used by `FilesView`'s "New mail
        with link" action — the same shape the in-Compose picker
        produces via `appendHtml`. */
    nextcloudLinks?: { filename: string; url: string }[]
    /** Nextcloud Talk room link to render into the body as a
        "Join the Talk room" block. Used by `TalkView`'s "Share link"
        action and by `MailView`'s "Create Talk room from this thread"
        action — the latter creates the room first and then opens
        Compose to invite the participants. */
    talkLink?: { name: string; url: string }
    /** Calendar-meeting invitation block (#195).  Set by
        App.svelte's "Respond with meeting" flow once the user
        saves the event in EventEditor — Compose then opens with
        a styled HTML invite card pre-filled in the body. */
    meetingInvite?: MeetingInvite
    /** When Compose is opened by clicking "Edit" on an existing draft
        in the Drafts folder, this points at the server-side copy we
        opened from. Once the user sends or re-saves, that copy needs
        to be expunged so the mailbox holds exactly one version of the
        message. `accountId` is snapshotted separately from the
        Compose-level `accountId` prop because the user might switch
        the From: picker mid-edit — we always want to expunge against
        the account the draft actually lives on, not whichever account
        the outgoing copy is now headed for. Unset for brand-new
        composes and replies/forwards. */
    draftSource?: { accountId: string; folder: string; uid: number }
  }

  /** Payload handed back to the parent when a background send
   *  fails — see `onsendfailed` below.  Carries everything the
   *  parent needs to reopen Compose pre-filled with the draft so
   *  the user doesn't lose their work (#156). */
  export interface SendFailurePayload {
    errorMessage: string
    draft: ComposeInitial
    fromAccountId: string
  }

  interface Props {
    /** Every configured mail account. Drives the From: picker; with
        a single account the picker collapses to a static label. */
    accounts: MailAccount[]
    /** The account this draft starts in — usually the active inbox
        account, or the receiving account for replies. The user can
        switch via the From: picker before sending. */
    accountId: string
    initial?: ComposeInitial
    onclose: () => void
    /** Fires when the background send pipeline (#156) hits an IMAP
     *  error after the modal has already closed.  The parent should
     *  reopen Compose with `payload.draft` so the user can retry
     *  without retyping.  Optional — when omitted, send failures
     *  after close are reported via console.warn only. */
    onsendfailed?: (payload: SendFailurePayload) => void
    /** Pre-populates the in-modal error banner — used by App.svelte
     *  when re-opening Compose after a background send failure so
     *  the reason the modal came back is visible right away. */
    initialError?: string
    /** True when Compose is the root of a popped-out standalone
        window (#110).  Hides the "Pop out" button (would just spawn
        another duplicate) and removes the modal overlay so the
        component fills the whole window. */
    inStandaloneWindow?: boolean
  }
  let {
    accounts,
    accountId,
    initial,
    onclose,
    onsendfailed,
    initialError = '',
    inStandaloneWindow = false,
  }: Props = $props()

  /**
   * Esc handler for the modal (#192).
   *
   * Triple-redundant after multiple attempts that should have
   * worked but didn't.  Root cause: when Compose opens, focus
   * stays on whichever button triggered it (compose button on
   * the IconRail / context-menu item / etc.) — that element
   * is *outside* Compose's wrapper, so the keydown event's
   * propagation path never crosses the wrapper.  Wrapper-
   * bound listeners (whether bubble or capture) silently
   * never fire.  Global window-level listeners *should* fire
   * for any focus location — and they do, except when
   * something earlier in the page swallows the event before
   * we get there (the suspect is some combination of Svelte
   * 5 `$effect` timing and Tiptap's ProseMirror keymap).
   *
   * Belt-and-suspenders fix:
   *   1. `bind:this={wrapperEl}` + a focus-on-mount $effect
   *      so the modal grabs focus immediately when it opens
   *      (which both moves focus into the wrapper for the
   *      onkeydown* handlers AND matches the conventional
   *      modal UX of "focus follows the modal").
   *   2. `onkeydowncapture={…}` on the wrapper itself, so
   *      capture-phase keystrokes that pass through the
   *      wrapper en-route to the focused element fire it
   *      before any descendant (Tiptap included).
   *   3. A separate global `window` capture listener via
   *      `addEventListener` inside `$effect` as a final
   *      fallback for anything we haven't anticipated.
   *
   * Each layer is independently sufficient under common
   * conditions; together they cover the corners.  Routes
   * through `cancel()` so the Talk-room (#86) and Nextcloud
   * share-link (#193) cleanups both fire.
   */
  let wrapperEl = $state<HTMLDivElement | undefined>()

  function onComposeKeydownCapture(e: KeyboardEvent) {
    if (e.key !== 'Escape') return
    if (showNcPicker || showNcImagePicker || showTalkModal) return
    if (document.querySelector('[role="listbox"]')) return
    e.preventDefault()
    cancel()
  }

  // Layer 1: focus the wrapper on mount so the modal owns
  // focus from the start.  `tabindex={-1}` on the wrapper
  // makes it programmatically focusable without showing up
  // in the tab order.  We only do this when there's no
  // explicit autofocus inside Compose (e.g. the To: field
  // for a fresh draft) — defensive `setTimeout(0)` so any
  // child autofocus runs first; if a child took focus we
  // leave it alone.
  $effect(() => {
    if (!wrapperEl) return
    const id = setTimeout(() => {
      // If focus is already inside the wrapper, don't steal it.
      if (wrapperEl?.contains(document.activeElement)) return
      wrapperEl?.focus({ preventScroll: true })
    }, 0)
    return () => clearTimeout(id)
  })

  // Layer 3: global window-capture listener.  Fires regardless
  // of where focus is — covers the path between Compose mount
  // and our autofocus actually landing, plus any future
  // refactor that keeps focus outside Compose.
  $effect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key !== 'Escape') return
      if (showNcPicker || showNcImagePicker || showTalkModal) return
      if (document.querySelector('[role="listbox"]')) return
      e.preventDefault()
      cancel()
    }
    window.addEventListener('keydown', onKey, { capture: true })
    return () =>
      window.removeEventListener('keydown', onKey, { capture: true })
  })

  // ── From: picker state ──────────────────────────────────────
  // The id is the canonical handle (used for `send_email`); the rest
  // of the form pulls the display name / email / signature from the
  // selected account. We re-resolve through `accounts` so that an
  // edit in settings (renamed account, updated signature) propagates
  // without remounting the modal.
  // svelte-ignore state_referenced_locally
  let fromAccountId = $state(accountId)
  const fromAccount = $derived(
    accounts.find((a) => a.id === fromAccountId) ?? accounts[0],
  )
  const fromAddress = $derived(fromAccount?.email ?? '')
  /** RFC-5322 From: header value, including the human's full
   *  name when set on the account.  Falls back to the bare
   *  email when neither `person_name` nor `display_name` is
   *  populated.  Quotes the name to keep characters like
   *  commas / periods from breaking the header parser on the
   *  receiving side. */
  const fromHeader = $derived.by(() => {
    if (!fromAccount) return fromAddress
    const name = (fromAccount.person_name ?? fromAccount.display_name ?? '').trim()
    if (!name) return fromAddress
    // Escape any internal quotes so the surrounding pair stays
    // balanced even if the user typed a `"` in their name.
    const escaped = name.replace(/"/g, '\\"')
    return `"${escaped}" <${fromAddress}>`
  })

  // ── Form state ──────────────────────────────────────────────
  // Seeded from `initial` (reply / forward / "share this file" entry
  // points) or blank. Drafts are no longer kept in localStorage —
  // persistence now goes through the Drafts IMAP folder via the
  // Save draft button, so there's nothing to rehydrate from here.

  /** Does this initial prefill carry quoted reply / forward content?
      Other prefills (FilesView attachments, TalkView links) leave the
      body empty, so the user is effectively starting a blank compose
      with extras and should still get the signature appended. */
  function isReplyOrForward(init?: ComposeInitial): boolean {
    return !!(init && (init.body || init.in_reply_to))
  }
  // svelte-ignore state_referenced_locally
  let to = $state(initial?.to ?? '')
  // svelte-ignore state_referenced_locally
  let cc = $state(initial?.cc ?? '')
  // svelte-ignore state_referenced_locally
  let bcc = $state(initial?.bcc ?? '')
  // svelte-ignore state_referenced_locally
  let subject = $state(initial?.subject ?? '')
  // svelte-ignore state_referenced_locally
  let body = $state(initial?.body ?? '')
  // svelte-ignore state_referenced_locally
  let attachments = $state<Attachment[]>(initial?.attachments ?? [])
  // Pre-warm thumb caches for any attachments seeded by the
  // host (FilesView's "New mail with attachment", reply-with-
  // attachment paths) so the `/` picker is instant on its
  // first open.
  $effect(() => {
    for (const a of initial?.attachments ?? []) {
      prewarmAttachmentThumb({
        bytes: a.data,
        contentType: a.content_type,
        filename: a.filename,
        cacheKey: a.content_id,
      })
    }
  })


  /** Human-readable label attached to every Nextcloud share minted
   *  from this Compose instance (#91).  Used to give each share an
   *  audit trail in the Nextcloud "Shared with others" view —
   *  "shared with bob@…, alice@…" rather than the default
   *  auto-generated name.  Empty when the user hasn't typed any
   *  recipients yet; we hand that through as `null` so Nextcloud's
   *  default naming kicks in.  Truncated at 96 chars because the
   *  OCS label field has a length limit and most share-list UIs
   *  ellipsize anything longer anyway. */
  let shareLabel = $derived.by(() => {
    const recipients = [to, cc, bcc].map((v) => v.trim()).filter(Boolean).join(', ')
    if (!recipients) return ''
    const prefix = 'For: '
    const max = 96
    return recipients.length + prefix.length <= max
      ? `${prefix}${recipients}`
      : `${prefix}${recipients.slice(0, max - prefix.length - 1)}…`
  })

  /** Extra tabs we contribute to the editor's ribbon (#103
   *  follow-up).  Wrapped in a `$derived` because the snippet
   *  symbols aren't bound until after the template has parsed —
   *  declaring this as a plain `let` would capture `undefined`. */
  let composeExtraTabs = $derived<ExtraTab[]>([
    {
      id: 'attach',
      label: 'Attach',
      iconName: 'attachment',
      content: attachTabContent,
    },
    {
      id: 'meetings',
      label: 'Meetings',
      iconName: 'meetings',
      content: meetingsTabContent,
    },
  ])

  /** Shares minted from this Compose during the current draft.
   *  We hold onto each one's `id` + `ncId` so when the recipient
   *  fields change *after* the share has been created, an effect
   *  below can re-PUT the new label.  Otherwise the audit trail
   *  in Nextcloud's "Shared with others" list freezes whatever
   *  the recipients were when the user clicked Share. */
  let createdShares = $state<ShareLink[]>([])

  // Debounced label-update effect: when `shareLabel` changes AND we
  // already have minted shares, push the new label.  Debounced so
  // typing the recipient list doesn't hammer the OCS endpoint —
  // 800ms is comfortable for "user has stopped typing" on a To line.
  $effect(() => {
    // Track the dependencies explicitly so Svelte re-runs the effect
    // when either side changes.
    const label = shareLabel
    const shares = createdShares
    if (shares.length === 0) return
    const handle = setTimeout(() => {
      for (const s of shares) {
        invoke('update_nextcloud_share_label', {
          ncId: s.ncId,
          shareId: s.id,
          label,
        }).catch((e) => {
          console.warn('update_nextcloud_share_label failed for share', s.id, e)
        })
      }
    }, 800)
    return () => clearTimeout(handle)
  })

  // Whether the Nextcloud file picker modal is mounted. Picker is lazy
  // so we don't hit `get_nextcloud_accounts` / PROPFIND until the user
  // actually clicks "Attach from Nextcloud".
  let showNcPicker = $state(false)
  // Separate flag for the *image* flow. Same `NextcloudFilePicker`
  // component but the `onpicked` handler differs: instead of
  // appending bytes to the attachments list, we base64-encode them
  // into a `data:` URL and insert as an inline `<img>`. Kept as a
  // distinct boolean so the two pickers can never be open at once
  // and neither flow silently steals the other's result.
  let showNcImagePicker = $state(false)
  // Imperative handle into the rich-text editor — populated once the
  // editor mounts. We use it to append Nextcloud share links into the
  // body without disturbing the user's cursor or undo history.
  // `$state` (rather than a plain `let`) is load-bearing here: the
  // signature `$effect` below depends on this becoming non-null to
  // know when to append. With a plain `let`, the effect's first run
  // happens before the child `RichTextEditor`'s `onready` fires, it
  // sees `editorApi` still null and exits — and because plain `let`s
  // aren't tracked, it never re-runs when the handle is finally set.
  // Making it `$state` subscribes the effect to the assignment.
  let editorApi = $state<EditorApi | null>(null)

  /** The exact signature HTML currently embedded in `bodyHtml`.
      `null` means none has been inserted yet — either the user
      has no signature configured, or `initialBodyHtml` ran before
      the accounts list resolved (the signature `$effect` will
      then stamp one in on first frame). Tracked so the From:
      picker can find-and-replace the signature when the user
      switches accounts mid-compose without clobbering content
      they've typed around it. Declared above the `bodyHtml`
      state so `initialBodyHtml` can write to it during its
      synchronous `$state` init. */
  let insertedSignatureHtml: string | null = null

  // The editor content as HTML — kept in sync via the RichTextEditor's
  // onchange callback. The initial body (from reply/forward/draft) is
  // plain text, so we convert newlines to <br> for the WYSIWYG view.
  // svelte-ignore state_referenced_locally
  let bodyHtml = $state(initialBodyHtml())

  // ── Attachment-mention warning (#250) ────────────────────────
  // When the user writes "attachment" / "Anhang" / "anbei" /
  // similar in the body but hasn't actually attached a file, we
  // surface a small warning banner above the editor.  Goes away
  // automatically once a file is attached, or when the user
  // dismisses it explicitly.  Per-Compose-instance state — a
  // fresh draft starts with the dismissal cleared so a previous
  // draft's "I don't care" doesn't leak across.
  let attachmentMentionDismissed = $state(false)
  /** Plain-text projection of `bodyHtml` for the mention
   *  detector.  Two reply-quote shapes have to be removed
   *  *structurally* before scanning, since neither is "the
   *  user's own new text" — finding "attached" inside a
   *  quoted thread we're replying to should never fire the
   *  warning:
   *
   *    1. `<div data-nimbus-block="quoted-history">…</div>` —
   *       the wrapper Compose stamps around the quoted
   *       original message on reply / forward.  Wraps a
   *       multi-paragraph block, so the closing `</div>` is
   *       far away from the opener and a regex can't match
   *       it reliably.
   *    2. `<blockquote>…</blockquote>` — the standard HTML
   *       reply-quote, used when the user pastes / quotes
   *       text manually via the editor's blockquote button.
   *
   *  We use DOMParser (browser-native, no dep) to walk the
   *  tree and remove the matching elements, then take the
   *  remaining text content.  Newlines are inserted at
   *  natural block boundaries so the mention detector's
   *  word-boundary regex doesn't accidentally bridge two
   *  paragraphs into one match.  */
  function bodyToPlainTextForMentionCheck(html: string): string {
    if (!html) return ''
    try {
      const doc = new DOMParser().parseFromString(html, 'text/html')
      doc
        .querySelectorAll('div[data-nimbus-block="quoted-history"], blockquote')
        .forEach((el) => el.remove())
      // Inject `\n` at block-element boundaries before grabbing
      // textContent, so a paragraph break in the source HTML
      // becomes a newline in the plain-text output.
      doc.querySelectorAll('br').forEach((el) => el.replaceWith(doc.createTextNode('\n')))
      doc
        .querySelectorAll('p, div, li, tr, h1, h2, h3, h4, h5, h6')
        .forEach((el) => el.append(doc.createTextNode('\n')))
      return (doc.body.textContent ?? '').trim()
    } catch {
      // DOMParser is part of the standard Web platform but
      // some test / non-browser harnesses don't ship it.  Fall
      // back to the raw HTML so the detector still gets to
      // run; the worst case is a stale-quoted-text false
      // positive in that environment, which is strictly
      // better than crashing the editor.
      return html
    }
  }
  const showAttachmentMentionWarning = $derived(
    !attachmentMentionDismissed
      && attachments.length === 0
      && mentionsAttachment(bodyToPlainTextForMentionCheck(bodyHtml)),
  )

  // Cards (Talk + meeting + quoted-history) all live IN the
  // editor's HTML body (#195 follow-up²). The RichTextEditor's
  // `NimbusBlock` Tiptap extension recognises every
  // `<div data-nimbus-block="…">` wrapper and renders it via a
  // NodeView so the styled markup survives schema parsing.
  // The chrome carries no `<img>` (we tried both a remote URL
  // and an inline data URI; mail clients ate both), so the
  // submit pipeline doesn't need any URL rewriting — what's in
  // the editor is what the recipient gets.
  function bodyHtmlForSubmission(): string {
    return bodyHtml
  }

  /** Build the editor's starting HTML.  Layout for replies and
      composes that carry an invite card / quote:

         1. two empty paragraphs — the user's cursor lands here,
            so there's always room to type at the top
         2. signature — sits above the cards so the user's name
            attaches to the new content, not to the recipient's
            quoted thread at the bottom
         3. invite cards (meeting / Talk)
         4. Nextcloud share-link block (when the user picked
            files in the in-Compose picker, same shape the
            `onlinks` callback emits)
         5. body — plain-text reply or the styled
            `<div data-nimbus-block="quoted-history">` for replies

      Drafts re-opened from the Drafts folder skip steps 1+2 —
      the saved HTML already contains whatever the user had at
      the time, so we render that verbatim rather than
      double-stamping breaklines or a signature. */
  function initialBodyHtml(): string {
    if (initial?.draftSource) {
      return textToHtml(body)
    }

    let html = textToHtml(body)
    const esc = (s: string) =>
      s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')

    let lead = '<p></p><p></p>'

    // Signature: best-effort grab of the active account at init
    // time so replies open with the user's name already attached.
    // If the account list hasn't loaded yet, we skip and let the
    // signature `$effect` append on first frame instead.
    const initSig = signatureBlock(
      (accounts.find((a) => a.id === accountId) ?? accounts[0])?.signature,
    )
    if (initSig) {
      lead += initSig
      insertedSignatureHtml = initSig
    }

    if (initial?.meetingInvite) lead += meetingInviteHtml(initial.meetingInvite)
    if (initial?.talkLink) lead += talkInviteHtml(initial.talkLink)
    if (initial?.nextcloudLinks && initial.nextcloudLinks.length > 0) {
      const items = initial.nextcloudLinks
        .map((l) => `<p>🔗 <a href="${l.url}">${esc(l.filename)}</a></p>`)
        .join('')
      lead += `<p><strong>Shared via Nextcloud:</strong></p>${items}`
    }

    return lead + html
  }

  /** Insert (or swap) the active account's signature when the editor
      is live and `fromAccount` is settled. The signature for the
      initial fromAccount is normally baked in at init time by
      `initialBodyHtml` (see step 2 of its layout); this effect
      handles the two cases that init can't:

        1. The accounts list hadn't loaded when `initialBodyHtml`
           ran, so no signature got stamped — first valid
           `fromAccount` triggers an insert at the start of the
           body so the layout still reads "breaklines → signature
           → cards → quote".
        2. The user picks a different From: account after the
           editor's already up — we find the previously-inserted
           signature anywhere in the body and splice in the new
           one (the original tail-only check broke once signature
           moved out of the trailing position into the middle of
           the document). */
  $effect(() => {
    if (!editorApi || !fromAccount) return
    const nextSig = signatureBlock(fromAccount.signature)

    if (insertedSignatureHtml === null) {
      if (!nextSig) return
      // Splice the signature right after the leading two empty
      // paragraphs that initialBodyHtml stamps in.  Falls back to
      // appendHtml when the body shape is unfamiliar (drafts).
      const leadIdx = bodyHtml.indexOf('<p></p><p></p>')
      if (leadIdx !== -1) {
        const after = leadIdx + '<p></p><p></p>'.length
        const replaced = bodyHtml.slice(0, after) + nextSig + bodyHtml.slice(after)
        editorApi.setHtml(replaced)
        bodyHtml = replaced
      } else {
        editorApi.appendHtml(nextSig)
      }
      insertedSignatureHtml = nextSig
      return
    }

    if (nextSig === insertedSignatureHtml) return

    // Account changed (or the user edited their signature in
    // settings while compose was open).  Find the previously-
    // inserted signature anywhere in the body and replace it.
    // Falling back to no-op when the user has clearly edited
    // around it (the substring is gone) is safer than trying to
    // guess a new insertion point.
    const idx = bodyHtml.indexOf(insertedSignatureHtml)
    if (idx === -1) return
    const replaced =
      bodyHtml.slice(0, idx) +
      nextSig +
      bodyHtml.slice(idx + insertedSignatureHtml.length)
    editorApi.setHtml(replaced)
    bodyHtml = replaced
    insertedSignatureHtml = nextSig || null
  })

  /** Render a per-account signature as the standard `-- ` separator
      followed by the user's lines. Returns `''` when there's no
      signature configured so callers can append unconditionally. */
  function signatureBlock(sig: string | null | undefined): string {
    const text = (sig ?? '').trim()
    if (!text) return ''
    const esc = (s: string) =>
      s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    const lines = text.split('\n').map(esc).join('<br>')
    // The "-- " (with trailing space) prefix is the RFC 3676 signature
    // delimiter — well-behaved mail clients hide it from quoted replies.
    return `<p>-- <br>${lines}</p>`
  }
  // svelte-ignore state_referenced_locally
  let showCcBcc = $state(!!cc || !!bcc)

  /** Naively convert plain text (with newlines) into minimal HTML. */
  function textToHtml(text: string): string {
    if (!text) return ''
    // If it already looks like HTML (from a draft/forward), return as-is.
    if (/<[a-z][\s\S]*>/i.test(text)) return text
    // Escape & < > so they render literally, then convert line breaks.
    return text
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/\n/g, '<br>')
  }

  /** Strip HTML tags to produce a plain-text fallback for `body_text`.
   *
   * `textContent` alone loses all line structure because it ignores
   * block boundaries and void elements. We pre-process the HTML to
   * insert `\n` at every meaningful break point before stripping, so
   * paragraphs and line breaks survive as plain-text newlines.
   */
  function htmlToText(html: string): string {
    const tmp = document.createElement('div')
    tmp.innerHTML = html
      .replace(/<br\s*\/?>/gi, '\n')
      .replace(/<\/p>/gi, '\n')
      .replace(/<\/div>/gi, '\n')
      .replace(/<\/li>/gi, '\n')
      .replace(/<\/tr>/gi, '\n')
    return (tmp.textContent ?? tmp.innerText ?? '')
      .replace(/\n{3,}/g, '\n\n')
      .trim()
  }

  /** Turn a Tauri-IPC byte array (`number[]`) into a `data:` URL
   *  the editor can embed as an `<img src>`. We route through a
   *  `Blob` + `FileReader` rather than `btoa(...)` so large images
   *  don't blow the JS stack on the `String.fromCharCode(...bytes)`
   *  apply-size limit, and the browser handles the base64 encode
   *  natively. Returns a promise because `FileReader` is async. */
  function bytesToDataUrl(bytes: number[], mime: string): Promise<string> {
    const blob = new Blob([new Uint8Array(bytes)], { type: mime })
    return new Promise((resolve, reject) => {
      const r = new FileReader()
      r.onload = () => resolve(r.result as string)
      r.onerror = () => reject(r.error)
      r.readAsDataURL(blob)
    })
  }

  let sending = $state(false)
  // `initialError` seeds the banner when Compose is re-opened
  // after a background send failure (#156).  Cleared by the next
  // `send()` validation pass — the user retrying is the implicit
  // dismissal.  `untrack` so we read the prop's value once at
  // mount without making `error` reactively follow the prop
  // (the parent doesn't update it after open, and Svelte 5's
  // `state_referenced_locally` warns when you naively pipe a
  // prop into a $state initialiser).
  let error = $state(untrack(() => initialError))

  // ── Talk room creation from Compose ────────────────────────
  // The "Add Event" flow used to live here too — it's been removed
  // now that calendar events are created exclusively from
  // CalendarView and Nextcloud's iMIP plugin (NC 30+ Mail Provider)
  // handles outbound invitation mail server-side.  Composing a
  // standalone iMIP attachment from a regular email is a footgun:
  // the event never lands in CalDAV, so RSVPs from attendees can't
  // pair back to anything on the organiser's side.  Talk rooms
  // remain a Compose-side feature — they're a meeting-link helper,
  // not a calendar entry.
  let showTalkModal = $state(false)
  let ncAccountId = $state('')
  /** The Talk room created during this compose session (if any).
      Used solely for body-block injection of the join link — no
      longer threaded into a calendar event from this surface. */
  let createdTalkLink = $state<{ name: string; url: string } | null>(null)
  /** Token of the room behind `createdTalkLink` — needed by
      `add_talk_participant` when we sync recipients back into the
      room on Send. */
  let talkRoomToken: string | null = null
  /** Tracks the room's current public/private state so the
      after-send sync can downgrade to private without an extra
      round-trip when every recipient turned out internal. */
  let talkRoomIsPublic: boolean = false
  /** In-session cache of "is this address an internal NC user?"
      so a recipient typed in both To and Cc only costs a single
      sharees lookup.  `null` value = looked up, no match. */
  type InternalUserHit = { user_id: string; display_name: string }
  const internalLookup = new Map<string, InternalUserHit | null>()
  async function resolveInternal(
    ncId: string,
    addr: string,
  ): Promise<InternalUserHit | null> {
    const key = addr.toLowerCase()
    if (internalLookup.has(key)) return internalLookup.get(key) ?? null
    try {
      const m = await invoke<InternalUserHit | null>('find_nextcloud_user_by_email', {
        ncId,
        email: addr,
      })
      internalLookup.set(key, m ?? null)
      return m ?? null
    } catch (e) {
      console.warn('find_nextcloud_user_by_email failed for', addr, e)
      internalLookup.set(key, null)
      return null
    }
  }
  /** Lower-cased bare addresses we've already POSTed to Talk's
      participant endpoint, so the post-save sync skips them. */
  const talkRoomParticipants = new Set<string>()
  /** Bare addresses the user wants invited to the Talk room but we
      haven't POSTed yet (#86: defer until Send so a discarded draft
      doesn't leak invites).  Compose accumulates these from the
      modal flow; `send()` calls `add_talk_participants` for the lot
      once the mail actually sends.  `cancel()` ignores them — the
      room itself gets DELETEd so any pending invites are moot. */
  let pendingTalkParticipants = $state<string[]>([])
  // (Removed `talkLinkInjected` flag — the previous editor-
  // injection path needed dedupe to avoid duplicate <p>💬 …</p>
  // blocks. With Talk invites now held as `pendingTalkInvite`
  // state and rendered in the preview, re-setting is idempotent,
  // so the flag is dead weight.)

  /** Resolve a Nextcloud account id (cached for the rest of the
      session). Sets `error` and returns null if none configured. */
  async function ensureNextcloudAccount(): Promise<string | null> {
    if (ncAccountId) return ncAccountId
    try {
      const accounts = await invoke<NextcloudAccount[]>('get_nextcloud_accounts')
      if (accounts.length === 0) {
        error = 'Connect a Nextcloud account first (Settings → Nextcloud).'
        return null
      }
      ncAccountId = accounts[0].id
      return ncAccountId
    } catch (e) {
      error = formatError(e) || 'Failed to load Nextcloud accounts'
      return null
    }
  }

  async function openTalkModal() {
    error = ''
    const id = await ensureNextcloudAccount()
    if (id) showTalkModal = true
  }

  // ── Stage-on-send meeting event (#152) ──────────────────────
  // The "Event" button mounts EventEditor in `stage` mode.  On
  // save we cache the editor's draft here; the actual CalDAV
  // create runs only after `send_email` succeeds, so a discarded
  // mail leaves no orphaned event behind.  Talk room (when the
  // user opted in) IS minted up-front because its URL has to
  // embed in the body — Compose's existing cancel-time Talk
  // cleanup (line ~1465) handles rollback automatically because
  // the EventEditor records the new room into the same
  // `talkRoomToken` slot.
  interface StagedMeetingEvent {
    summary: string
    start: string
    end: string
    attendees: string[]
    location?: string | null
    description?: string | null
    calendarId: string
    /** Opaque payload that round-trips into
     *  `create_calendar_event` on send-success — same shape the
     *  editor builds in create mode.  Typed `unknown` because
     *  Compose doesn't need to introspect it. */
    input: unknown
  }
  let stagedEvent = $state<StagedMeetingEvent | null>(null)
  let showEventEditor = $state(false)
  let editorCalendars = $state<CalendarSummary[]>([])
  let editorDraftSeed = $state<{
    calendarId: string
    start: Date
    end: Date
    summary?: string
    attendees?: string[]
    createTalkRoom?: boolean
  } | null>(null)

  /** Round-up helper used to seed a fresh event's start time at
   *  the next half-hour boundary — same convention the
   *  "Respond with meeting" flow in App.svelte uses. */
  function nextHalfHour(d: Date): Date {
    const n = new Date(d)
    n.setSeconds(0, 0)
    const m = n.getMinutes()
    if (m === 0 || m === 30) {
      n.setMinutes(m + 30)
    } else if (m < 30) {
      n.setMinutes(30)
    } else {
      n.setMinutes(0)
      n.setHours(n.getHours() + 1)
    }
    return n
  }

  async function openEventEditor() {
    error = ''
    const ncId = await ensureNextcloudAccount()
    if (!ncId) return
    let cals: CalendarSummary[] = []
    try {
      cals = await invoke<CalendarSummary[]>('get_cached_calendars', { ncId })
    } catch (e) {
      error = formatError(e) || 'Failed to load calendars'
      return
    }
    const visible = cals.filter((c) => !c.hidden)
    if (visible.length === 0) {
      error = 'No writable calendars found on your Nextcloud account.'
      return
    }
    let initialCalendarId = visible[0].id
    try {
      const s = await invoke<{ default_calendar_id: string | null }>('get_app_settings')
      if (s.default_calendar_id && visible.some((c) => c.id === s.default_calendar_id)) {
        initialCalendarId = s.default_calendar_id!
      }
    } catch {
      // Best-effort — fall back to first visible calendar.
    }
    const start = nextHalfHour(new Date())
    const end = new Date(start.getTime() + 30 * 60 * 1000)

    editorCalendars = visible
    editorDraftSeed = {
      calendarId: initialCalendarId,
      start,
      end,
      summary: subject.trim() || undefined,
      attendees: recipients(),
      // Auto-mint Talk room on open.  Matches the spirit of the
      // existing "Respond with meeting" flow (where Talk is on
      // by default for thread-replies); the user can untick the
      // box inside EventEditor if they don't want one.
      createTalkRoom: false,
    }
    showEventEditor = true
  }

  function onEventEditorClose() {
    showEventEditor = false
    editorDraftSeed = null
  }

  function onEventEditorSaved(saved?: SavedEvent) {
    showEventEditor = false
    editorDraftSeed = null
    if (!saved || !saved.input || !saved.calendarId) return
    stagedEvent = {
      summary: saved.summary,
      start: saved.start,
      end: saved.end,
      attendees: saved.attendees,
      location: saved.location ?? null,
      description: saved.description ?? null,
      calendarId: saved.calendarId,
      input: saved.input,
    }
    // Render the meeting card into the body so the recipient
    // sees an inline summary.  Splits Talk URL out of `location`
    // the same way App.svelte's "Respond with meeting" handler
    // does — keeps the styled "Join Talk meeting" CTA rendering
    // when the user opted into Talk.
    const loc = (saved.location ?? '').trim()
    const isUrl = /^https?:\/\//i.test(loc)
    const invite: MeetingInvite = {
      summary: saved.summary,
      start: saved.start,
      end: saved.end,
      location: isUrl ? null : loc || null,
      description: saved.description ?? null,
      talkUrl: isUrl ? loc : null,
    }
    const html = meetingInviteHtml(invite)
    if (editorApi) {
      editorApi.insertBeforeNimbusBlock(html, 'quoted-history')
    }
  }

  /** Combined To + Cc list as bare/RFC-formatted address strings,
      ready to seed CreateTalkRoomModal / EventEditor's attendee inputs. */
  function recipients(): string[] {
    return [...splitAddrs(to), ...splitAddrs(cc)]
  }

  /** Insert a Talk invite card into the editor body when the
      user creates a Talk room mid-compose.  The new
      `insertBeforeNimbusBlock` editor API drops the card just
      above any existing quoted-history block (so reading order
      is card → reply text → previous conversation); on a fresh
      compose with no quote it appends at the end. The card is
      parsed by the editor's `NimbusBlock` extension into an
      atom node, so a single Backspace deletes the whole thing
      if the user changes their mind. */
  function injectTalkBlock(link: { name: string; url: string }) {
    const html = talkInviteHtml(link)
    if (editorApi) {
      editorApi.insertBeforeNimbusBlock(html, 'quoted-history')
    }
  }

  function onTalkRoomCreated(room: TalkRoom, participants: string[]) {
    createdTalkLink = { name: room.display_name, url: room.web_url }
    talkRoomToken = room.token
    // Subject ↔ Talk-room name sync.  The modal pre-fills its
    // "Room name" field from `subject` already, so the
    // subject-filled-first case lands correctly server-side.  When
    // the user opened the modal with an empty subject and typed a
    // room name there, copy it back into the subject so the
    // outgoing mail isn't subject-less.  We only auto-fill — we
    // never overwrite a subject the user already typed (that would
    // clobber a deliberate "subject != room name" choice).
    if (!subject.trim() && room.display_name.trim()) {
      subject = room.display_name
    }
    // The modal is mounted with `deferParticipants={true}`, so the
    // Talk room itself was minted empty.  Stash the entered list
    // here; `send()` POSTs them to Talk once the mail actually goes
    // out.  Cancel-as-discard wipes the room (and these invites
    // along with it), so `pendingTalkParticipants` is the audit
    // trail for "what we said we'd invite".
    for (const p of participants) {
      const k = bareAddr(p).toLowerCase()
      if (k && !talkRoomParticipants.has(k)) {
        pendingTalkParticipants = [...pendingTalkParticipants, p]
      }
    }
    // Keep the mail recipients in sync with the Talk invite: any
    // address the user typed into the modal that wasn't already on
    // To/Cc/Bcc gets added to To.
    mergeIntoRecipients(participants)
    // Talk button is a "share this room now" gesture so the body
    // block goes in immediately.  The Add-Event auto-create path
    // bypasses this callback (uses `createTalkRoomSilently`) so its
    // block can be deferred until the event is saved.
    injectTalkBlock(createdTalkLink)
  }

  /**
   * Create a Talk room without going through CreateTalkRoomModal —
   * used by the Add-Event path so the auto-created room's URL is
   * available to prefill the event's URL field. The body block is
   * **not** injected here; that happens after the event is saved
   * (per the user-facing "after the event is saved, add the talk
   * link to the mail body" semantics).
   */
  async function createTalkRoomSilently() {
    const id = ncAccountId
    if (!id) return
    const seen = new Set<string>()
    const dedupd: { kind: 'email'; value: string }[] = []
    for (const r of recipients()) {
      const addr = bareAddr(r)
      if (!addr) continue
      const k = addr.toLowerCase()
      if (seen.has(k)) continue
      seen.add(k)
      dedupd.push({ kind: 'email', value: addr })
    }
    const room = await invoke<TalkRoom>('create_talk_room', {
      ncId: id,
      // Talk requires a non-empty name. Fall back to a generic label
      // when the user hasn't set a subject yet — they can rename in
      // Nextcloud later.
      roomName: subject.trim() || '(meeting)',
      // #86: defer participant invites until Send.  The room is
      // minted empty here; the recipient list rides along in
      // `pendingTalkParticipants` so `send()` can flush the lot once
      // the mail actually goes out.
      participants: [],
      // #124: mint as public so externals can join via the link in
      // the body.  The send-time sync downgrades to private once we
      // confirm every recipient is an internal NC user (mirrors
      // EventEditor's policy — same UX in both surfaces).
      roomType: 3,
    })
    createdTalkLink = { name: room.display_name, url: room.web_url }
    talkRoomToken = room.token
    talkRoomIsPublic = true
    for (const p of dedupd) {
      const k = p.value.toLowerCase()
      if (!talkRoomParticipants.has(k) && !pendingTalkParticipants.some((x) => bareAddr(x).toLowerCase() === k)) {
        pendingTalkParticipants = [...pendingTalkParticipants, p.value]
      }
    }
  }


  /** Persist the current compose state to the account's IMAP Drafts
      folder via `save_draft` on the backend. The draft lands in the
      Drafts mailbox (visible across devices); the compose modal then
      closes. `sending` gates the button so a second click mid-flight
      doesn't double-APPEND the same draft. */
  async function saveDraft() {
    error = ''
    sending = true
    try {
      // `replaceSource` lets the backend APPEND + EXPUNGE in a single
      // IMAP session — critical for "edit an existing draft" because
      // two separate commands were sometimes leaving the original
      // copy behind (server hadn't flushed the APPEND before the
      // DELETE ran on a fresh connection). Letting the backend batch
      // the two also guarantees APPEND and DELETE target the *same*
      // folder (the one the user opened the draft from), even on
      // servers where `pick_drafts_folder` would otherwise choose a
      // different `\Drafts`-attributed mailbox than the one the user
      // is looking at.
      const src = initial?.draftSource
      await invoke('save_draft', {
        accountId: src?.accountId ?? fromAccountId,
        email: {
          from: fromHeader,
          to: splitAddrs(to),
          cc: splitAddrs(cc),
          bcc: splitAddrs(bcc),
          reply_to: null,
          subject,
          body_text: htmlToText(bodyHtmlForSubmission()),
          body_html: bodyHtmlForSubmission() || null,
          attachments,
        },
        replaceSource: src ? { folder: src.folder, uid: src.uid } : null,
      })
      // Disarm the share-cleanup branch in cancel() (#193): the
      // draft we just persisted has the share URLs baked into its
      // body, so the user expects them to keep working when they
      // resume the draft later.  Without this, closing the modal
      // after Save Draft would call cancel() (no — onclose() is
      // direct here, but matching the send() pattern keeps the
      // disarm idiom consistent in case the close path changes).
      createdShares = []
      onclose()
    } catch (e: any) {
      error = formatError(e) || 'Failed to save draft'
    } finally {
      sending = false
    }
  }

  // Split a comma/semicolon-separated address list into trimmed addresses.
  function splitAddrs(s: string): string[] {
    return s
      .split(/[,;]/)
      .map((a) => a.trim())
      .filter(Boolean)
  }

  /** Strip an `"Name" <addr>` wrapper down to the bare address. Same
      parser shape the EventEditor / CreateTalkRoomModal use. */
  function bareAddr(piece: string): string {
    const trimmed = piece.trim()
    if (!trimmed) return ''
    const m = trimmed.match(/^\s*(?:"[^"]*"|[^<]*?)\s*<([^>]+)>\s*$/)
    return m ? m[1].trim() : trimmed
  }

  /** Same parser as `bareAddr`, but also pulls the display name out
      of the `"Name" <addr>` wrapper so the @ picker can show the
      friendly form for participants the user has typed without a
      matching address-book entry. */
  function parseAddress(piece: string): { name: string; email: string } {
    const trimmed = piece.trim()
    if (!trimmed) return { name: '', email: '' }
    const m = trimmed.match(/^\s*"?([^"<]*?)"?\s*<([^>]+)>\s*$/)
    if (m) return { name: m[1].trim(), email: m[2].trim() }
    return { name: '', email: trimmed }
  }

  // ── Contact picker (`@` mention) data plumbing ─────────────
  // Cache the full address book once at mount; the `@` query then
  // filters in-memory. Cheap because contacts rarely number above
  // a few hundred per Nextcloud account, and the alternative
  // (round-tripping `search_contacts` on every keystroke) would
  // both add latency and complicate the participants-first
  // ordering we owe the user.
  interface ContactKindValue { kind: string; value: string }
  interface ContactRow {
    id: string
    nextcloud_account_id: string
    display_name: string
    email: ContactKindValue[]
    phone: ContactKindValue[]
    organization: string | null
    photo_mime: string | null
  }
  let allContacts = $state<ContactRow[]>([])
  $effect(() => {
    invoke<ContactRow[]>('get_contacts')
      .then((rows) => {
        allContacts = rows
      })
      .catch((e) => {
        // No Nextcloud accounts / sync hasn't run / etc. — we
        // degrade to "participants only" silently rather than
        // breaking the @ flow with an error toast.
        console.warn('get_contacts failed (mention picker continues with participants only):', e)
      })
  })

  /** Materialize the parts of the contact-row Tiptap needs. The
   *  full ContactRow has multiple emails per contact; the picker
   *  is per-email so each address gets its own row. */
  function suggestionFor(c: ContactRow, email: string): ContactSuggestion {
    return {
      id: email,
      label: c.display_name || email,
      email,
      photoUrl: c.photo_mime ? convertFileSrc(c.id, 'contact-photo') : null,
      hint: c.organization ?? null,
    }
  }

  /** Hand the editor's `@` picker the two-tier list:
   *  - `participants`: addresses the user has already added to To/Cc
   *    (in that order, deduped). These show first in the popup so
   *    the most likely target — someone already on the mail — is
   *    one keystroke away.
   *  - `others`: address-book matches that aren't already a
   *    participant. Capped to 8 to keep the popup tight. */
  async function contactQuery(query: string) {
    const q = query.trim().toLowerCase()
    const participantEmails = new Set<string>()
    const participants: ContactSuggestion[] = []

    for (const field of [to, cc]) {
      for (const piece of splitAddrs(field)) {
        const { name, email } = parseAddress(piece)
        if (!email) continue
        const key = email.toLowerCase()
        if (participantEmails.has(key)) continue
        participantEmails.add(key)
        // Enrich with the address-book row if we have one — gives
        // the row a photo + organization hint, and a real display
        // name when the user typed only the bare address.
        const full = allContacts.find((c) =>
          c.email.some((e) => e.value.toLowerCase() === key),
        )
        if (full) {
          participants.push(suggestionFor(full, email))
        } else {
          participants.push({
            id: email,
            label: name || email,
            email,
            photoUrl: null,
            hint: null,
          })
        }
      }
    }

    const matchesQuery = (s: ContactSuggestion) =>
      !q || s.label.toLowerCase().includes(q) || s.email.toLowerCase().includes(q)
    const filteredParticipants = participants.filter(matchesQuery)

    const others: ContactSuggestion[] = []
    const seenOthers = new Set<string>()
    for (const c of allContacts) {
      for (const e of c.email) {
        const email = e.value
        const key = email.toLowerCase()
        if (!email || participantEmails.has(key) || seenOthers.has(key)) continue
        const sug = suggestionFor(c, email)
        if (!matchesQuery(sug)) continue
        seenOthers.add(key)
        others.push(sug)
        if (others.length >= 8) break
      }
      if (others.length >= 8) break
    }

    return { participants: filteredParticipants, others }
  }

  /** Auto-add the picked contact to Cc when the email isn't already
      somewhere on the recipient list. Issue #61 specifically asks
      for Cc (not To) so a tangential `@`-mention doesn't promote
      a side reference into a primary recipient. */
  function onContactPicked(c: ContactSuggestion) {
    const seen = new Set<string>()
    for (const f of [to, cc, bcc]) {
      for (const piece of splitAddrs(f)) {
        const e = bareAddr(piece).toLowerCase()
        if (e) seen.add(e)
      }
    }
    if (seen.has(c.email.toLowerCase())) return
    const formatted = c.label && c.label !== c.email
      ? `"${c.label.replace(/"/g, '\\"')}" <${c.email}>`
      : c.email
    const trimmed = cc.trim()
    cc = trimmed
      ? `${trimmed}${trimmed.endsWith(',') ? ' ' : ', '}${formatted}, `
      : `${formatted}, `
    // Make sure the user can see the Cc field they were just
    // auto-credited into — otherwise the change feels invisible.
    showCcBcc = true
  }

  /**
   * Merge newly invited addresses back into the email's To field.
   * Used by both the Talk-room-created and calendar-event-saved
   * callbacks so adding someone in either modal also adds them to
   * the mail recipients — keeping the invite and the email aligned.
   * Deduplication is case-insensitive on the bare address, and we
   * skip addresses that are already on To/Cc/Bcc so the user never
   * gets surprised by a recipient jumping from Cc back to To.
   */
  function mergeIntoRecipients(addresses: string[]) {
    const have = new Set<string>()
    for (const field of [to, cc, bcc]) {
      for (const a of splitAddrs(field)) {
        const bare = bareAddr(a).toLowerCase()
        if (bare) have.add(bare)
      }
    }
    const additions: string[] = []
    for (const a of addresses) {
      const bare = bareAddr(a).toLowerCase()
      if (!bare || have.has(bare)) continue
      have.add(bare)
      additions.push(a)
    }
    if (additions.length === 0) return
    const current = splitAddrs(to)
    to = [...current, ...additions].join(', ')
  }

  /** Generate a stable, collision-resistant RFC 2392 Content-ID for
      a freshly picked attachment. We tag every attachment the user
      adds so the `/` shortcut in the editor can reference any of
      them — even the ones the user ultimately doesn't link to just
      carry an unused id, which is cheap. UUID v4 from the
      browser's Web Crypto API; we strip the hyphens so the id
      round-trips cleanly through headers without extra quoting. */
  function makeContentId(): string {
    return crypto.randomUUID().replaceAll('-', '')
  }

  async function onPickFiles(e: Event) {
    const input = e.target as HTMLInputElement
    if (!input.files) return
    const picked: Attachment[] = []
    for (const file of Array.from(input.files)) {
      const buf = await file.arrayBuffer()
      picked.push({
        filename: file.name,
        content_type: file.type || 'application/octet-stream',
        data: Array.from(new Uint8Array(buf)),
        content_id: makeContentId(),
      })
    }
    attachments = [...attachments, ...picked]
    // Pre-warm the thumbnail cache off the critical path so the
    // editor's `/` picker opens to fully-rendered tiles instead
    // of icons that progressively swap in.  Keyed by the stable
    // content_id string so the picker's `thumbUrlSync` lookup
    // hits regardless of Svelte's $state proxy wrapping the
    // bytes array (which would change object identity and miss
    // a WeakMap-by-ref lookup).
    for (const a of picked) {
      prewarmAttachmentThumb({
        bytes: a.data,
        contentType: a.content_type,
        filename: a.filename,
        cacheKey: a.content_id,
      })
    }
    input.value = ''
  }

  /** #162 — let the user click an attachment chip to preview
   *  what they're about to send.  Same dispatch as the receive
   *  side: Office/PDF/Markdown go to a viewer window, anything
   *  else opens in the OS default desktop app.  Bytes are
   *  already in memory (`att.data`), so no IPC round-trip is
   *  needed before the open. */
  async function previewAttachment(att: Attachment) {
    try {
      await openAttachment(
        { filename: att.filename, content_type: att.content_type },
        async () => att.data,
      )
    } catch (e: any) {
      console.warn('preview attachment failed', e)
      error = e?.message ?? 'Failed to open attachment'
    }
  }

  function removeAttachment(i: number) {
    attachments = attachments.filter((_, idx) => idx !== i)
  }

  async function send() {
    error = ''
    const toList = splitAddrs(to)
    if (toList.length === 0) {
      error = 'At least one recipient is required.'
      return
    }

    // #156: close the modal immediately and run the IMAP
    // submission in the background.  Big attachments take 10+
    // seconds; keeping the user staring at a "Sending…" button
    // for that whole window is exactly what the issue asks us to
    // stop.  Snapshot every value the post-validation pipeline
    // reads from component scope BEFORE `onclose()` so the work
    // survives the unmount cleanly.
    const snap = {
      fromAccountId,
      fromHeader,
      to,
      cc,
      bcc,
      subject,
      body,
      bodyHtml: bodyHtmlForSubmission(),
      toList,
      ccList: splitAddrs(cc),
      bccList: splitAddrs(bcc),
      attachments: [...attachments],
      talkRoomToken,
      talkRoomIsPublic,
      talkRoomParticipantsCopy: new Set(talkRoomParticipants),
      ncAccountId,
      accountsAtSend: accounts,
      initialAtSend: initial,
      draftSource: initial?.draftSource ?? null,
      // #152 — staged meeting event.  Snapshotted here so the
      // background pipeline can fire `create_calendar_event`
      // *after* `send_email` succeeds.  We clear the in-scope
      // `stagedEvent` below to disarm any cancel-time rollback.
      stagedEvent,
    }

    // Disarm the delete-on-discard cleanups *now* — once the modal
    // closes, `cancel()` won't run again, but neither will any
    // future state mutation here propagate.  Clearing these in
    // component scope keeps any stray re-render quiet.
    //
    // Talk room: the recipient is about to be invited via the
    // background submission below, so we mustn't tear the room
    // down on close.
    // Share links (#193): the email body carries the URLs, so the
    // recipient needs the shares to keep working — clear the
    // tracking list so cancel()'s cleanup branch is a no-op.
    talkRoomToken = null
    pendingTalkParticipants = []
    createdShares = []
    // #152 — disarm any cancel-time rollback for the staged
    // event; the background pipeline will create it on
    // send-success.
    stagedEvent = null

    onclose()

    // Defer the heavy bits past the next macrotask so Svelte
    // gets to flush the unmount + browser gets to paint before
    // we hit `invoke('send_email', …)`.  Without this gap, the
    // attachment payload gets structured-cloned on the same
    // task as the click handler, freezing the close animation
    // for 200-800 ms with multi-MB attachments.  The `requestAnimationFrame`
    // ensures we paint the empty state, then `setTimeout 0`
    // hands control to the next macrotask where the IPC starts.
    requestAnimationFrame(() => {
      setTimeout(() => {
        void runSendPipeline(snap)
      }, 0)
    })
  }

  /** Background continuation of `send()` — runs after the modal
   *  has closed.  On any IMAP error fires `onsendfailed` with a
   *  draft-shaped snapshot so the parent can re-open Compose
   *  pre-filled with everything the user typed.  Talk-room
   *  invites and draft expunge are best-effort; their failures
   *  go to console.warn since the modal that used to host the
   *  in-line warning is gone. */
  async function runSendPipeline(snap: {
    fromAccountId: string
    fromHeader: string
    to: string
    cc: string
    bcc: string
    subject: string
    body: string
    bodyHtml: string
    toList: string[]
    ccList: string[]
    bccList: string[]
    attachments: Attachment[]
    talkRoomToken: string | null
    talkRoomIsPublic: boolean
    talkRoomParticipantsCopy: Set<string>
    ncAccountId: string | null
    accountsAtSend: MailAccount[]
    initialAtSend: ComposeInitial | undefined
    draftSource: { accountId: string; folder: string; uid: number } | null
    stagedEvent: StagedMeetingEvent | null
  }): Promise<void> {
    try {
      await invoke('send_email', {
        accountId: snap.fromAccountId,
        email: {
          from: snap.fromHeader,
          to: snap.toList,
          cc: snap.ccList,
          bcc: snap.bccList,
          reply_to: null,
          subject: snap.subject,
          body_text: htmlToText(snap.bodyHtml),
          body_html: snap.bodyHtml || null,
          attachments: snap.attachments,
        },
      })
    } catch (e: any) {
      const msg = formatError(e) || 'Failed to send'
      console.warn('send_email failed (modal already closed)', e)
      onsendfailed?.({
        errorMessage: msg,
        draft: {
          to: snap.to,
          cc: snap.cc,
          bcc: snap.bcc,
          subject: snap.subject,
          body: snap.body,
          attachments: snap.attachments,
          in_reply_to: snap.initialAtSend?.in_reply_to ?? null,
          nextcloudLinks: snap.initialAtSend?.nextcloudLinks,
          talkLink: snap.initialAtSend?.talkLink,
          draftSource: snap.draftSource ?? undefined,
        },
        fromAccountId: snap.fromAccountId,
      })
      return
    }

    // Flush deferred Talk-room invites (#86).  Best-effort: a
    // failure here doesn't surface anywhere user-visible now
    // that the modal is gone, but the participants still got
    // the join URL in the body so the room itself remains
    // usable.  See the original send() comment block for the
    // full rationale on Bcc exclusion etc.
    if (snap.talkRoomToken && snap.ncAccountId) {
      const ncId = snap.ncAccountId
      const room = snap.talkRoomToken
      const seen = new Set<string>()
      const participantsToAdd: (
        | { kind: 'email'; value: string }
        | { kind: 'user'; value: string }
      )[] = []
      let allInternal = true
      const senderIdentities = new Set<string>()
      for (const a of snap.accountsAtSend) {
        if (a.email) senderIdentities.add(a.email.toLowerCase())
      }
      try {
        const profileEmail = await invoke<string | null>(
          'get_nextcloud_user_email',
          { ncId },
        )
        if (profileEmail) senderIdentities.add(profileEmail.toLowerCase())
      } catch (e) {
        console.warn('get_nextcloud_user_email failed', e)
      }
      for (const raw of [...snap.toList, ...snap.ccList]) {
        const addr = bareAddr(raw)
        if (!addr) continue
        const key = addr.toLowerCase()
        if (seen.has(key) || snap.talkRoomParticipantsCopy.has(key)) continue
        if (senderIdentities.has(key)) continue
        seen.add(key)
        const match = await resolveInternal(ncId, addr)
        if (match) {
          participantsToAdd.push({ kind: 'user', value: match.user_id })
          continue
        }
        allInternal = false
        participantsToAdd.push({ kind: 'email', value: addr })
      }
      if (participantsToAdd.length > 0) {
        try {
          await invoke('add_talk_participants', {
            ncId,
            roomToken: room,
            participants: participantsToAdd,
          })
        } catch (e) {
          console.warn('add_talk_participants after send failed', e)
        }
        if (allInternal && snap.talkRoomIsPublic) {
          try {
            await invoke('set_talk_room_public', {
              ncId,
              roomToken: room,
              public: false,
            })
          } catch (e) {
            console.warn('set_talk_room_public(false) failed', e)
          }
        }
      }
    }

    // #152 — create the staged calendar event now that the
    // mail has actually shipped.  Best-effort; a failure here
    // leaves the email already sent (the user can re-create
    // the event manually if they care) but doesn't undo the
    // send.  The Talk room (if minted) is already public + has
    // the user-supplied URL embedded in the body, so the event
    // creation finishing up is purely a "now schedule it"
    // step.
    if (snap.stagedEvent) {
      try {
        await invoke('create_calendar_event', {
          calendarId: snap.stagedEvent.calendarId,
          input: snap.stagedEvent.input,
        })
      } catch (e) {
        console.warn('create_calendar_event after send failed', e)
      }
    }

    // Expunge the original Drafts copy so the mailbox holds
    // exactly one version of the message.  Best-effort post-
    // close: a failure leaves a stale draft which the user can
    // discard manually, but it doesn't undo the send.
    if (snap.draftSource) {
      try {
        await invoke('delete_message', {
          accountId: snap.draftSource.accountId,
          folder: snap.draftSource.folder,
          uid: snap.draftSource.uid,
        })
      } catch (e) {
        console.warn('expunge draft after background send failed', e)
      }
    }
  }

  /** Snapshot the current Compose state into a popout payload, open
   *  a standalone Compose window with it, and dismiss this modal.
   *  The popped-out window's `Compose` mounts with the same fields
   *  the user has typed so far — so this is "move my draft into a
   *  separate window" rather than "open a fresh blank Compose".
   *  Hidden when we're already inside the standalone window. */
  async function popoutCompose() {
    try {
      await openComposeInStandaloneWindow({
        accountId: fromAccountId,
        initial: {
          to,
          cc,
          bcc,
          subject,
          body,
          attachments,
          // Preserve the reply / draft / external-share context the
          // current modal was opened with so the popped-out window
          // continues to behave as a reply / draft edit / etc.
          in_reply_to: initial?.in_reply_to ?? null,
          nextcloudLinks: initial?.nextcloudLinks,
          talkLink: initial?.talkLink,
          draftSource: initial?.draftSource,
        },
      })
    } catch (e) {
      console.warn('openComposeInStandaloneWindow failed', e)
      return
    }
    // Close the modal silently — the popped-out window owns the
    // state now.  `onclose` triggers the parent's refresh bump
    // which is harmless here.
    onclose()
  }

  function cancel() {
    // No local persistence — if the user wants to resume later they
    // need to click "Save draft" first (which APPENDs to IMAP Drafts).
    //
    // Clean up any Talk room minted during this draft (#86).  The
    // room was created empty (deferred invites), so a DELETE is
    // safe — no recipients will see a "you've been removed" notice.
    // If the user already pressed Send, `talkRoomToken` was nulled
    // out in `send()`, so the room is left alone.
    if (talkRoomToken && ncAccountId) {
      const ncId = ncAccountId
      const room = talkRoomToken
      // Fire-and-forget: a failure here is annoying (orphan room in
      // Nextcloud) but not worth blocking the close on.  The user
      // can clean it up from Talk manually.
      invoke('delete_talk_room', { ncId, roomToken: room }).catch((e) => {
        console.warn('delete_talk_room on cancel failed', e)
      })
      talkRoomToken = null
    }
    // Clean up any Nextcloud share links the user minted during this
    // draft (#193).  Same rationale as the Talk-room cleanup above:
    // shares only make sense once the mail goes out.  Send and
    // Save-draft both clear `createdShares` to disarm this branch
    // (the recipient still needs the link in those flows); explicit
    // cancel is the only path that leaves orphan shares behind, so
    // we delete them here.  Fire-and-forget — a failure leaves a
    // stray share in the user's "Shared with others" list, which
    // they can clean up manually but isn't worth blocking on.
    //
    // We dedupe two sources:
    //   * `createdShares` — minted *this* Compose session.
    //   * `extractManagedShares(bodyHtml)` — markers stamped into
    //     anchors by previous sessions (the user opened an
    //     existing draft and is now cancelling it).
    const shareKeys = new Set<string>()
    const sharesToDelete: { ncId: string; shareId: string }[] = []
    for (const s of createdShares) {
      const key = `${s.ncId}::${s.id}`
      if (shareKeys.has(key)) continue
      shareKeys.add(key)
      sharesToDelete.push({ ncId: s.ncId, shareId: s.id })
    }
    for (const m of extractManagedShares(bodyHtml)) {
      const key = `${m.ncId}::${m.shareId}`
      if (shareKeys.has(key)) continue
      shareKeys.add(key)
      sharesToDelete.push(m)
    }
    createdShares = []
    for (const s of sharesToDelete) {
      invoke('delete_nextcloud_share', {
        ncId: s.ncId,
        shareId: s.shareId,
      }).catch((e) => {
        console.warn('delete_nextcloud_share on cancel failed for', s.shareId, e)
      })
    }
    onclose()
  }

  /** Expunge the server-side draft the user opened Compose from, if
      any. Called after a successful send or re-save so a single
      editing session can't leave orphan copies piling up in the
      Drafts folder. A failure here means the new copy made it but the
      old one didn't — we surface a clear hint so the user knows to
      clean up manually (and so we notice the bug in testing) rather
      than silently ending up with two copies. */
  async function expungeDraftSource(): Promise<string | null> {
    const src = initial?.draftSource
    if (!src) return null
    try {
      await invoke('delete_message', {
        accountId: src.accountId,
        folder: src.folder,
        uid: src.uid,
      })
      return null
    } catch (e) {
      console.warn('Failed to delete source draft:', e)
      return formatError(e) || 'Failed to remove the old draft copy'
    }
  }
</script>

<!-- In standalone-window mode we drop the modal overlay + the fixed
     resizable card and let Compose fill the whole window.  The OS
     window itself is the resizable container.  The body of the form
     is shared via the `composeBody` snippet so we don't duplicate
     several hundred lines of template across the two branches. -->
{#if inStandaloneWindow}
  <div
    bind:this={wrapperEl}
    class="h-full w-full flex flex-col bg-surface-50 dark:bg-surface-900 outline-none"
    role="dialog"
    aria-modal="false"
    tabindex="-1"
    onkeydowncapture={onComposeKeydownCapture}
  >
    {@render composeBody()}
  </div>
{:else}
  <div
    bind:this={wrapperEl}
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50 outline-none"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onkeydowncapture={onComposeKeydownCapture}
  >
    <!-- Resizable via native CSS `resize: both`. We seed a comfortable
         720 × 80vh default and then constrain:
           min-w/h — keep the form usable once labels and toolbar fit;
           max-w/h — always leave 5vw of breathing room around the edges
                     so the dialog doesn't clip under the title bar.
         `overflow: hidden` is required for the resize handle to appear
         (browsers only show it on overflow-managed elements); the inner
         flex-column already scrolls the body region, so the modal
         itself never needs to scroll. -->
    <div
      class="compose-modal bg-surface-50 dark:bg-surface-900 rounded-lg shadow-xl flex flex-col"
      style="resize: both; overflow: hidden; width: 720px; height: 80vh; min-width: 480px; min-height: 420px; max-width: 95vw; max-height: 95vh;"
    >
      {@render composeBody()}
    </div>
  </div>
{/if}

{#snippet composeBody()}
    <header class="px-5 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center justify-between gap-2">
      <h2 class="text-base font-semibold">New message</h2>
      <div class="flex items-center gap-2">
        {#if !inStandaloneWindow}
          <!-- Pop the modal out into its own resizable window (#110).
               Hidden inside the standalone window itself — there's
               nothing to pop out of when you're already a window. -->
          <button
            class="btn btn-sm preset-outlined-surface-500 text-xs inline-flex items-center gap-1.5"
            onclick={() => void popoutCompose()}
            title="Open this draft in a separate window"
          ><Icon name="full-screen" size={14} /> Pop out</button>
        {/if}
        <button class="text-surface-500 hover:text-surface-900 dark:hover:text-surface-100" onclick={cancel} aria-label="Close">✕</button>
      </div>
    </header>

    <!-- Body is a flex column so the RichTextEditor slot can claim
         `flex-1` and grow as the user resizes the modal taller.
         `min-h-0` is the usual incantation that lets a flex child
         actually shrink below its content size; without it the editor
         would never release height back to the container. We drop the
         outer `overflow-y-auto` because the editor manages its own
         internal scroll, and swap `space-y-3` (margins don't play well
         with `flex-1` siblings) for `gap-3`. -->
    <div class="flex-1 min-h-0 flex flex-col p-5 gap-3">
      <!-- From: picker. Shown as a real <select> when the user has
           more than one account, otherwise collapsed to a static label
           so single-account users see no extra chrome. -->
      <div class="flex items-center gap-2">
        <label class="text-xs w-14 text-surface-500" for="compose-from">From</label>
        {#if accounts.length > 1}
          <select
            id="compose-from"
            class="select flex-1 px-3 py-2 text-sm rounded-md"
            bind:value={fromAccountId}
          >
            {#each accounts as a (a.id)}
              <option value={a.id}>{a.display_name || a.email} &lt;{a.email}&gt;</option>
            {/each}
          </select>
        {:else}
          <span id="compose-from" class="text-sm text-surface-700 dark:text-surface-300">
            {(fromAccount?.person_name ?? fromAccount?.display_name) || ''} &lt;{fromAddress}&gt;
          </span>
        {/if}
      </div>

      <div class="flex items-center gap-2">
        <label class="text-xs w-14 text-surface-500" for="compose-to">To</label>
        <AddressAutocomplete
          id="compose-to"
          bind:value={to}
          placeholder="alice@example.com, bob@example.com"
        />
        {#if !showCcBcc}
          <button class="text-xs text-primary-500 hover:underline" onclick={() => (showCcBcc = true)}>Cc/Bcc</button>
        {/if}
      </div>

      {#if showCcBcc}
        <div class="flex items-center gap-2">
          <label class="text-xs w-14 text-surface-500" for="compose-cc">Cc</label>
          <AddressAutocomplete id="compose-cc" bind:value={cc} />
        </div>
        <div class="flex items-center gap-2">
          <label class="text-xs w-14 text-surface-500" for="compose-bcc">Bcc</label>
          <AddressAutocomplete id="compose-bcc" bind:value={bcc} />
        </div>
      {/if}

      <div class="flex items-center gap-2">
        <label class="text-xs w-14 text-surface-500" for="compose-subject">Subject</label>
        <input id="compose-subject" class="input flex-1 px-3 py-2 text-sm rounded-md" bind:value={subject} />
      </div>

      <!-- `flex-1 min-h-0` makes this the stretchy slot in the body
           column; the editor inside uses `h-full` to fill it.

           The trailing-actions snippet hands the editor toolbar
           our Attach / Talk / Files / Send / Save Draft / Discard
           buttons so they share one toolbar row with the rich-text
           controls instead of living in a separate footer (#103). -->
      <div class="flex-1 min-h-0 flex flex-col">
        <RichTextEditor
          content={bodyHtml}
          onchange={(html) => { bodyHtml = html }}
          onready={(api) => { editorApi = api }}
          onrequestncimage={() => { showNcImagePicker = true }}
          oncontactquery={contactQuery}
          oncontactpicked={onContactPicked}
          attachmentsForRef={attachments
            .filter((a): a is Attachment & { content_id: string } => !!a.content_id)
            .map((a) => ({
              content_id: a.content_id,
              filename: a.filename,
              content_type: a.content_type,
              data: a.data,
            }))}
          actionsTrailing={sendActions}
          extraTabs={composeExtraTabs}
        />
      </div>

      <!-- #250 — gentle "you mentioned an attachment but didn't
           attach a file" warning.  Hides itself the moment any
           file is attached; user can also dismiss it explicitly
           if their mention is intentional (e.g. "as we discussed
           in the attached PDF I sent earlier"). -->
      {#if showAttachmentMentionWarning}
        <div
          class="flex items-start gap-3 px-3 py-2 rounded-md border border-warning-500/40 bg-warning-500/10 text-sm text-warning-700 dark:text-warning-300"
          role="alert"
        >
          <span aria-hidden="true">⚠️</span>
          <div class="flex-1 min-w-0">
            <p class="font-medium">{m.compose_attachment_warning_title()}</p>
            <p class="text-xs mt-0.5">{m.compose_attachment_warning_body()}</p>
          </div>
          <button
            type="button"
            class="btn btn-sm preset-outlined-warning-500 shrink-0"
            onclick={() => (attachmentMentionDismissed = true)}
          >{m.compose_attachment_warning_dismiss()}</button>
        </div>
      {/if}

      {#if attachments.length > 0}
        <div class="flex flex-wrap gap-2">
          {#each attachments as att, i (i)}
            <!-- #162 — chip is clickable; opens the file in
                 the same viewer the receive side uses (Office /
                 PDF / Markdown in a webview, everything else in
                 the OS default app).  The ✕ remove button keeps
                 its own click handler with stopPropagation so
                 hitting it doesn't also trigger the preview. -->
            <span class="inline-flex items-center gap-2 px-2 py-1 rounded-md bg-surface-200 dark:bg-surface-800 text-xs">
              <button
                type="button"
                class="inline-flex items-center gap-2 cursor-pointer text-left"
                onclick={() => void previewAttachment(att)}
                title="Preview {att.filename}"
              >
                <AttachmentThumb
                  bytes={att.data}
                  contentType={att.content_type}
                  filename={att.filename}
                  cacheKey={att.content_id}
                  class="w-7 h-7"
                />
                <span>{att.filename}</span>
              </button>
              <button
                class="text-surface-500 hover:text-red-500"
                onclick={(e) => {
                  e.stopPropagation()
                  removeAttachment(i)
                }}
                aria-label="Remove"
              >✕</button>
            </span>
          {/each}
        </div>
      {/if}

      {#if error}
        <p class="text-sm text-red-500">{error}</p>
      {/if}
    </div>

{/snippet}

<!--
  Compose's contribution to the editor's tab strip + tab-content
  area (#103).  We register an "Attach" tab so Attach / Files /
  Talk / Event live alongside the editor's Format / Insert /
  Layout tabs in the same ribbon, AND a tab-strip-trailing snippet
  for the Save / Discard / Send actions which stay visible
  regardless of which tab is open (the standard "always-on
  primary actions in the ribbon header" pattern).
-->

<!-- Attach tab panel — local + Nextcloud file pickers.  Same
     `.rt-btn` styling the editor's panels use so each Compose tab
     reads as visually indistinguishable from a built-in. -->
{#snippet attachTabContent()}
  <label class="rt-btn cursor-pointer" title="Attach a file from your computer">
    <span class="rt-btn-icon"><Icon name="attachment" size={20} /></span>
    <span class="rt-btn-label">Attach</span>
    <input type="file" multiple class="hidden" onchange={onPickFiles} />
  </label>
  <button
    type="button"
    class="rt-btn"
    title="Attach a file or link from Nextcloud"
    onclick={() => (showNcPicker = true)}
  >
    <span class="rt-btn-icon"><Icon name="cloud" size={20} /></span>
    <span class="rt-btn-label">NC Files</span>
  </button>
{/snippet}

<!-- Meetings tab panel — Nextcloud Talk + calendar-event creation.
     Split out from Attach so picking a meeting feature isn't
     buried in a "files" context. -->
{#snippet meetingsTabContent()}
  <button
    type="button"
    class="rt-btn"
    title="Create a Nextcloud Talk room with the current recipients"
    onclick={openTalkModal}
  >
    <span class="rt-btn-icon"><Icon name="meetings" size={20} /></span>
    <span class="rt-btn-label">Talk</span>
  </button>
  <!-- #152 — Event button: opens EventEditor in stage mode.  The
       calendar event itself isn't created until the user
       successfully sends the mail; an unsent draft leaves no
       orphaned event.  When the user opts into "Make it a Talk
       conversation" inside the editor the Talk room IS minted up
       front (the URL has to land in the body), and Compose's
       cancel-time Talk cleanup deletes that room if the mail is
       discarded. -->
  <button
    type="button"
    class="rt-btn"
    title="Create a calendar event with the current recipients (saved on send)"
    disabled={!!stagedEvent}
    onclick={() => void openEventEditor()}
  >
    <span class="rt-btn-icon"><Icon name="calendar" size={20} /></span>
    <span class="rt-btn-label">Event</span>
  </button>
{/snippet}

<!-- Always-visible Save / Discard / Send actions in the tab-strip
     trailing slot.  Compact (`.ctb`) so they fit alongside the tab
     buttons.  Send is primary-filled and rightmost. -->
{#snippet sendActions()}
  <button
    type="button"
    class="ctb"
    disabled={sending}
    title="Save the current draft to the Drafts folder"
    onclick={saveDraft}
  >
    <span class="ctb-icon"><Icon name="save-draft" size={16} /></span>
    <span class="ctb-label">Save</span>
  </button>
  <button
    type="button"
    class="ctb ctb-danger"
    disabled={sending}
    title="Discard this draft and close the window"
    onclick={cancel}
  >
    <span class="ctb-icon"><Icon name="trash" size={16} /></span>
    <span class="ctb-label">Discard</span>
  </button>
  <button
    type="button"
    class="ctb ctb-primary"
    disabled={sending}
    title="Send the message"
    onclick={send}
  >
    <span class="ctb-icon"><Icon name="sent" size={16} /></span>
    <span class="ctb-label">{sending ? 'Sending…' : 'Send'}</span>
  </button>
{/snippet}

{#if showNcPicker}
  <NextcloudFilePicker
    {shareLabel}
    onpicked={(picked) => {
      // Stamp a content_id on each newly-arrived attachment so the
      // `/` editor shortcut can reference it — the Nextcloud picker
      // doesn't carry the field in its own `Attachment` shape, so
      // Compose is the earliest point where we can assign one.
      const stamped = picked.map((a) => ({ ...a, content_id: makeContentId() }))
      attachments = [...attachments, ...stamped]
      for (const a of stamped) {
        prewarmAttachmentThumb({
          bytes: a.data,
          contentType: a.content_type,
          filename: a.filename,
          cacheKey: a.content_id,
        })
      }
    }}
    onlinks={(links) => {
      // Track every share that's been minted from this Compose so a
      // later edit of To / Cc / Bcc can re-PUT a fresh `For: …`
      // label onto each one (#91 follow-up).  See `$effect` below.
      createdShares = [...createdShares, ...links]
      // Drop a small "Shared via Nextcloud" block at the end of the
      // message body. Each link is its own paragraph so it survives
      // mail clients that strip styling. We escape the filename text
      // (URLs themselves only need href-quoting, not body-escaping).
      const esc = (s: string) =>
        s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      // Tag each anchor with the share's id + owning Nextcloud
      // account id (#193).  When the draft is later deleted (from
      // the mail list, or via the explicit Cancel after reopen)
      // the cleanup pass scans the body for these markers and
      // calls `delete_nextcloud_share` for each one — without the
      // markers there'd be no way to map a `https://…/s/<token>`
      // URL back to the share record we need to delete.
      const items = links
        .map(
          (l) =>
            `<p>🔗 <a href="${l.url}" data-nimbus-share-id="${esc(l.id)}" data-nimbus-share-nc="${esc(l.ncId)}">${esc(l.filename)}</a></p>`,
        )
        .join('')
      const block = `<p><strong>Shared via Nextcloud:</strong></p>${items}`
      // Splice the block ABOVE an auto-inserted signature when one
      // is sitting at the end of the body so the share renders
      // inline with the message rather than below the user's
      // sign-off.  Same pattern the Talk-link injection uses
      // earlier in this component.
      if (
        insertedSignatureHtml
        && bodyHtml.endsWith(insertedSignatureHtml)
        && editorApi
      ) {
        const without = bodyHtml.slice(0, bodyHtml.length - insertedSignatureHtml.length)
        const replaced = without + block + insertedSignatureHtml
        editorApi.setHtml(replaced)
        bodyHtml = replaced
      } else {
        editorApi?.appendHtml(block)
      }
    }}
    onclose={() => (showNcPicker = false)}
  />
{/if}

{#if showNcImagePicker}
  <!-- Image-insert flow: reuse the NC picker in "attach" mode, but
       instead of appending the bytes to the attachments list we
       inline them as `data:` URLs via the editor's `insertImage`.
       Non-image picks are ignored — the picker doesn't constrain
       file type server-side, so the UI-side filter is what keeps a
       stray `.docx` out of the body. -->
  <NextcloudFilePicker
    onpicked={async (picked) => {
      for (const p of picked) {
        if (!p.content_type.startsWith('image/')) continue
        const src = await bytesToDataUrl(p.data, p.content_type)
        editorApi?.insertImage(src)
      }
    }}
    onclose={() => (showNcImagePicker = false)}
  />
{/if}

{#if showTalkModal && ncAccountId}
  <CreateTalkRoomModal
    ncId={ncAccountId}
    initialName={subject}
    initialParticipants={recipients()}
    deferParticipants={true}
    onclose={() => (showTalkModal = false)}
    oncreated={onTalkRoomCreated}
  />
{/if}

<!-- #152 — EventEditor in stage mode for the "Event" button on
     the Meetings tab.  The editor mounts only when
     `showEventEditor` is true; on save we cache the staged
     details and create the actual CalDAV event after send. -->
{#if showEventEditor && editorDraftSeed}
  <EventEditor
    mode="stage"
    calendars={editorCalendars}
    draft={editorDraftSeed}
    onclose={onEventEditorClose}
    onsaved={onEventEditorSaved}
  />
{/if}

<style>
  /* Compose-toolbar buttons (#103).  Stacked icon + tiny label so
     each action stays compact in the unified toolbar row.  The
     `:global` is needed because these buttons render inside the
     RichTextEditor's `actionsTrailing` snippet — Svelte scopes
     parent styles to the parent's DOM, but snippet contents land
     in the child component's tree.  Variants:
       - `.ctb-primary` — the Send button (filled primary).
       - `.ctb-danger`  — the Discard button (red on hover). */
  :global(.ctb) {
    display: inline-flex;
    flex-direction: column;
    align-items: center;
    gap: 0.05rem;
    padding: 0.25rem 0.5rem;
    border-radius: 0.375rem;
    line-height: 1;
    color: inherit;
    cursor: pointer;
    transition: background-color 100ms ease, color 100ms ease;
  }
  :global(.ctb:hover:not(:disabled)) {
    background: rgb(0 0 0 / 0.06);
  }
  :global([data-mode='dark'] .ctb:hover:not(:disabled)) {
    background: rgb(255 255 255 / 0.08);
  }
  :global(.ctb:disabled) {
    opacity: 0.5;
    cursor: not-allowed;
  }
  :global(.ctb-icon) {
    font-size: 1rem;
  }
  :global(.ctb-label) {
    font-size: 0.625rem;
    line-height: 1;
    white-space: nowrap;
  }
  :global(.ctb-primary) {
    color: white;
    background: var(--color-primary-500, #3b82f6);
  }
  :global(.ctb-primary:hover:not(:disabled)) {
    background: var(--color-primary-600, #2563eb);
  }
  :global(.ctb-danger:hover:not(:disabled)) {
    color: rgb(239 68 68);
    background: rgb(239 68 68 / 0.12);
  }
</style>

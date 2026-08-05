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

  import * as api from './api'
  import DOMPurify from 'dompurify'
  import { formatError } from './errors'
  import { m } from '../paraglide/messages'
  import NextcloudFilePicker from './NextcloudFilePicker.svelte'
  import MoveFolderPicker from './MoveFolderPicker.svelte'
  import FileTypeIcon from './FileTypeIcon.svelte'
  import Icon from './Icon.svelte'
  import CryptoChips from './CryptoChips.svelte'
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
  import {
    applyInlineImages,
    buildInlineImageUrls,
    collectCidImageRefs,
    inlineImageBlob,
  } from './inlineImages'
  import { parseMailtoUrl } from './mailtoUrl'
  import { onDestroy } from 'svelte'

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
    /** Kebab-case protection tag from the receive path (#57).
     *  `"encrypted"` — message was PGP-encrypted; we decrypted it
     *  locally.  `"signed"` — `multipart/signed` outer (detection
     *  only for now).  `"signed-and-encrypted"` — both.
     *  `"encrypted-cannot-decrypt"` — encrypted message that the
     *  receive path couldn't unwrap (JMAP no-raw-blob fallback);
     *  the UI renders a banner instead of a chip.
     *  `null` for plaintext mail. */
    protection?: string | null
    /** Kebab-case verification outcome from the receive path
     *  (`"valid"` | `"invalid"` | `"unknown-signer"`).
     *  `null` for unsigned mail. */
    signature_status?: string | null
    /** Hex fingerprint of the verified signer.  Only present when
     *  `signature_status === "valid"`. */
    signer_fingerprint?: string | null
    /** Local-only pin state (#414), overlaid from the cache by the
     *  fetch paths. */
    is_pinned?: boolean
    /** Sender-declared priority from the `X-Priority:` /
     *  `Importance:` headers (#414): `'high'` / `'low'`, absent =
     *  normal. */
    priority?: string | null
    /** User-set priority override (#414): `'high'` / `'normal'` /
     *  `'low'`.  Wins over `priority` for the badge. */
    priority_override?: string | null
    /** RFC 5322 Message-ID (#277), bracket-free.  Used here to look
     *  up sent-mail receipt status (#416). */
    message_id?: string | null
    /** `Disposition-Notification-To:` value (#416) — the sender
     *  asked for a read receipt, addressed to this mailbox.  Absent
     *  for ordinary mail (and suppressed by the backend when it
     *  points at the reading account's own address). */
    mdn_requested_to?: string | null
    /** What already happened about that request (#416): `'sent'` |
     *  `'declined'`, absent = not yet decided.  Local-only overlay
     *  from the cache — keeps the banner from asking twice. */
    mdn_handled?: string | null
  }

  /** Receipt-tracking state for a sent mail (#416), from the
   *  `get_receipt_status` IPC.  `disposition: null` = requested but
   *  nothing back yet. */
  interface SentReceiptStatus {
    requested_at: number
    disposition: string | null
    disposition_at: number | null
    reporter: string | null
  }

  interface Props {
    accountId: string
    folder?: string
    uid: number | null
    onread?: (uid: number) => void
    /** Fire after the flag / pin / priority toggles (#414) commit so
        the parent can mutate the matching MailList envelope in place
        — same plumbing as `onread`.  MailList re-derives its row
        order from the mutation, so a pin flip re-sorts the list
        without a refetch. */
    onflagchanged?: (uid: number, flagged: boolean) => void
    onpinchanged?: (uid: number, pinned: boolean) => void
    onprioritychanged?: (uid: number, priority: string | null) => void
    /** Live mirror of the open message's mail-list row (#414
        follow-up).  When present, the flag / pin / priority toggles
        render from THIS instead of the locally-fetched `email` copy,
        so a toggle made in the list while the message is open
        updates the reading-pane buttons immediately.  The parent
        passes the same envelope object MailList mutates
        optimistically; `null` (standalone windows, no matching row)
        falls back to `email`'s own fields. */
    listState?: {
      is_starred?: boolean
      is_pinned?: boolean
      priority?: string | null
      priority_override?: string | null
    } | null
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
    /** Create a Nextcloud task seeded from this email (#92).
        The handler in `App.svelte` picks the NC account + first
        task list and writes a VTODO whose `URL` is the
        `mail://account/folder/uid` ref so the task carries a
        clickable backlink to the source message.  Mirrors
        `onsavenote`'s ergonomics — we hand over the email + its
        UID so the title / source-link are sourced from the
        currently-rendered message. */
    oncreatetask?: (mail: Email & { uid: number }) => void
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
    onflagchanged,
    onpinchanged,
    onprioritychanged,
    listState = null,
    onreply,
    onreplyall,
    onforward,
    onrespondwithmeeting,
    onsavenote,
    oncreatetask,
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

  /** #57 — PGP decrypt prompt state.  Lives in MailView (not in
   *  the chip subcomponent) so the IPC result swaps the local
   *  `email` state directly, no event plumbing.  All three are
   *  reset whenever the user opens a different message (see the
   *  `$effect` in `load()`) so a passphrase typed on one mail
   *  never leaks into another. */
  let decryptPassphrase = $state('')
  let decrypting = $state(false)
  let decryptError = $state('')

  /** #341 — passphrase held for the duration of the current
   *  message view, so attachment opens / downloads / prints /
   *  save-to-Nextcloud can re-decrypt the encrypted inner MIME
   *  tree without prompting per click.  Set on successful body
   *  decrypt (carried over from `decryptPassphrase` before the
   *  input is wiped); cleared whenever the open message changes
   *  via `load()` so the passphrase never outlives the mail it
   *  unlocked.  Separate from `decryptPassphrase` so the DOM
   *  input still gets wiped (no plaintext-in-input footprint)
   *  while attachments stay usable for the same message session. */
  let sessionPassphrase = $state('')

  async function runDecrypt() {
    if (!email || !decryptPassphrase || decrypting) {
      return
    }
    decrypting = true
    decryptError = ''
    try {
      // Pull `(account, folder, uid)` off the existing email
      // record — the message stays selected while we re-fetch,
      // so the props the parent component holds for `accountId`,
      // `folder`, and `uid` haven't changed.
      const uidNum = Number(email.id.split(':').pop())
      const decrypted = await api.crypto.decryptMessage({
        accountId: email.account_id,
        folder: email.folder,
        uid: uidNum,
        pgpPassphrase: decryptPassphrase,
      })
      email = decrypted
      // #341 — stash the passphrase so attachment fetches for this
      // same message session can use `download_decrypted_attachment`
      // without re-prompting.  Done BEFORE wiping the input so the
      // session value carries through.
      sessionPassphrase = decryptPassphrase
      // Clear the passphrase the moment the IPC resolves so it
      // never lingers on the heap or in a DOM input that the
      // user could leave open.
      decryptPassphrase = ''
    } catch (e: any) {
      // Strip the `Crypto: ` variant prefix from UnkaiError so the
      // user sees a clean sentence in the badge ("Wrong encryption
      // passphrase") rather than "Crypto: Wrong encryption
      // passphrase".  Log-side errors still carry the prefix via
      // the IPC channel, so debugging info isn't lost.
      const raw = formatError(e) || 'Decrypt failed'
      decryptError = raw.replace(/^Crypto:\s*/i, '')
    } finally {
      decrypting = false
    }
  }

  /** #341 — does the open message need the decrypt-aware attachment
   *  fetch path?  True when the receive layer tagged it as PGP-
   *  encrypted (either flavour); false for plaintext and
   *  signed-only mail.  `encrypted-cannot-decrypt` (JMAP fallback)
   *  also returns false because we can't decrypt locally anyway —
   *  the UI surfaces a different banner for that case. */
  function emailNeedsDecryptedAttachmentFetch(em: Email | null): boolean {
    const p = em?.protection
    return p === 'encrypted' || p === 'signed-and-encrypted'
  }

  /** #341 — does the open email's cached body look like it still
   *  needs decrypting?  Auto-decrypt-on-load reuses this to skip
   *  the keychain round-trip when the cache already holds the
   *  plaintext from a previous successful Decrypt. */
  function emailBodyLooksEncrypted(em: Email | null): boolean {
    if (!emailNeedsDecryptedAttachmentFetch(em)) return false
    const text = (em?.body_text ?? '').trim()
    const html = (em?.body_html ?? '').trim()
    return text.length === 0 && html.length === 0
  }

  /** #341 — mirrors `pgp_has_unlock_automatically` for the currently
   *  open account.  Refreshed alongside the message load below.
   *  Drives two things:
   *    - the attachment-fetch gate (`fetchAttachmentBytes` can let
   *      a fetch through with an empty session passphrase because
   *      the backend will resolve it via the keychain),
   *    - the auto-decrypt-on-load attempt (we only call
   *      `try_auto_decrypt_message` when the toggle is on, so
   *      opt-out accounts don't pay a wasted IPC). */
  let autoUnlockForAccount = $state(false)

  /** Sentinel error string — thrown by `fetchAttachmentBytes` when
   *  the message is encrypted but we don't have a session passphrase
   *  in hand.  Caught by the per-action wrappers below so the
   *  surfaced error reads as a clear UX hint ("re-enter passphrase")
   *  rather than a raw IPC error. */
  const DECRYPT_REQUIRED_MARKER = '__unkai_decrypt_required__'

  /** #341 — encryption-aware single source of truth for "give me the
   *  decoded bytes of attachment X on the open message".  Routes to
   *  `download_decrypted_attachment` (with the session passphrase)
   *  when the message is encrypted, and to the plain
   *  `download_email_attachment` otherwise.  Throws the
   *  `DECRYPT_REQUIRED_MARKER` sentinel when an encrypted message
   *  has no session passphrase available (e.g. the user opened it
   *  from cache without re-running the Decrypt flow) so each call
   *  site can surface a consistent "please re-decrypt first" hint
   *  without each one having to reproduce the conditional. */
  async function fetchAttachmentBytes(att: EmailAttachment): Promise<number[]> {
    if (!email || uid == null) {
      throw new Error('No message open')
    }
    if (emailNeedsDecryptedAttachmentFetch(email)) {
      // #341 — let the IPC through with an empty pgpPassphrase
      // when this account opted into "Unlock automatically"; the
      // backend resolves the passphrase via the OS keychain in
      // that case.  Only when *neither* a manually-typed session
      // passphrase nor an auto-unlock entry is in scope do we
      // surface the DECRYPT_REQUIRED_MARKER short-circuit, which
      // the wrapping handlers translate to a friendly "please
      // re-decrypt first" message.
      if (!sessionPassphrase && !autoUnlockForAccount) {
        throw new Error(DECRYPT_REQUIRED_MARKER)
      }
      return await api.crypto.downloadDecryptedAttachment({
        accountId: email.account_id,
        folder: email.folder,
        uid,
        partId: att.part_id,
        pgpPassphrase: sessionPassphrase,
      })
    }
    return await api.mail.downloadEmailAttachment({
      accountId: email.account_id,
      folder: email.folder,
      uid,
      partId: att.part_id,
    })
  }

  /** Convert the sentinel from `fetchAttachmentBytes` into the UI's
   *  inline-error string.  Other errors fall through with the normal
   *  `formatError` treatment. */
  function formatAttachmentFetchError(e: unknown, fallback: string): string {
    if (e instanceof Error && e.message === DECRYPT_REQUIRED_MARKER) {
      return m.mail_view_attachment_decrypt_required()
    }
    return formatError(e) || fallback
  }

  // ── Inline body images (#471) ────────────────────────────────
  //
  // `<img src="cid:logo@example">` points at one of the message's
  // own MIME parts.  The webview can't resolve that scheme, so
  // without this the images the sender meant to appear *in* the body
  // render as nothing — they only ever showed up as attachment chips.
  //
  // Flow: after a body lands, scan it for cid references; if there
  // are any, pull every referenceable image part in one IPC
  // (`fetch_inline_images` — one server round-trip for the whole
  // message, not one per image), turn each into an object URL, and
  // let `processEmailHtml` rewrite the sources.
  //
  // Unlike *remote* images these need no "Show images" opt-in: the
  // bytes are already inside the message, so rendering them phones
  // nobody home and leaks no read receipt.

  /** Lookup key (cid / filename, normalised) → object URL. */
  let inlineImageUrls = $state<Record<string, string>>({})

  /** True from "this body has cid images" until the fetch settles.
   *  Keeps unresolved images invisible while in flight instead of
   *  flashing broken-image icons for the length of a network fetch. */
  let inlineImagesLoading = $state(false)

  /** The object URLs we own, for revoking.  Deliberately not
   *  `$state` — nothing renders off it, and mutating it must not
   *  invalidate the body. */
  let inlineImageObjectUrls: string[] = []

  /** Which (message, body) we last fetched for, so the effect below
   *  doesn't refetch on every unrelated state change.  Plain `let`
   *  for the same reason. */
  let inlineImagesKey = ''

  function revokeInlineImageUrls() {
    for (const url of inlineImageObjectUrls) URL.revokeObjectURL(url)
    inlineImageObjectUrls = []
  }

  onDestroy(revokeInlineImageUrls)

  $effect(() => {
    const em = email
    const u = uid
    const html = em?.body_html ?? ''
    if (!em || u == null || !html) return
    // Body length is a cheap proxy for "the body changed" — the
    // case that matters is an encrypted message whose plaintext
    // arrives after the first render.
    const key = `${em.account_id}::${em.folder}::${u}::${html.length}`
    if (key === inlineImagesKey) return
    inlineImagesKey = key
    const doc = new DOMParser().parseFromString(html, 'text/html')
    if (collectCidImageRefs(doc).length === 0) {
      // Nothing inline — the overwhelmingly common case.  No IPC,
      // and drop the pending state so a previous message's flag
      // can't strand this body.
      inlineImagesLoading = false
      return
    }
    inlineImagesLoading = true
    void loadInlineImages(em, u)
  })

  /** Fetch and register the message's inline image parts.  Failures
   *  are swallowed to a console warning on purpose: a body image
   *  that won't load is a cosmetic problem, and an error banner over
   *  a readable message would be worse than the missing logo. */
  async function loadInlineImages(em: Email, u: number) {
    const encrypted = emailNeedsDecryptedAttachmentFetch(em)
    // Encrypted and no way to decrypt yet — the user hasn't entered
    // the passphrase for this message and the account isn't on
    // auto-unlock.  Decrypting sets a new body, which re-runs the
    // effect above with a fresh key, so this isn't a dead end.
    if (encrypted && !sessionPassphrase && !autoUnlockForAccount) {
      inlineImagesLoading = false
      return
    }
    try {
      const parts = await api.mail.fetchInlineImages({
        accountId: em.account_id,
        folder: em.folder,
        uid: u,
        // Empty string is meaningful for auto-unlock accounts (the
        // backend resolves the passphrase from the keychain); the
        // key must be absent entirely for plaintext mail.
        ...(encrypted ? { pgpPassphrase: sessionPassphrase } : {}),
      })
      // The user moved on while we were fetching — drop the result
      // rather than painting one message's images onto another.
      if (email?.id !== em.id) return
      const created: string[] = []
      const urls = buildInlineImageUrls(parts, (part) => {
        const url = URL.createObjectURL(inlineImageBlob(part))
        created.push(url)
        return url
      })
      revokeInlineImageUrls()
      inlineImageObjectUrls = created
      inlineImageUrls = urls
    } catch (e) {
      console.warn('fetch_inline_images failed', e)
    } finally {
      if (email?.id === em.id) inlineImagesLoading = false
    }
  }

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
      const rows = await api.mail.getAttachmentPreviews(
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
          ? await api.mail.downloadEmailAttachment({
              accountId: cur.account_id,
              folder: cur.folder,
              uid: curUid,
              partId: att.part_id,
            })
          : await api.calendar.downloadCalendarFromMessage({
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
        const summary = await api.calendar.parseEventInvite({ bytes })
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
          void api.calendar.recordCancelledInvite({ uid: summary.uid }).catch(
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
    // #471 — inline images belong to exactly one message.  Start in
    // the "loading" state so the first paint of a body with cid
    // images doesn't flash the unresolved-image treatment before the
    // effect has even had a chance to look at it.
    revokeInlineImageUrls()
    inlineImageUrls = {}
    inlineImagesLoading = true
    inlineImagesKey = ''
    // #57 — clear any in-flight decrypt state from the previous
    // message; a passphrase the user typed for `mail A` must not
    // ride along to `mail B`.
    decryptPassphrase = ''
    decryptError = ''
    decrypting = false
    // #341 — drop the session passphrase too.  Held only for the
    // currently-open message so attachment fetches can decrypt
    // without re-prompting; opening a different message starts a
    // fresh session.
    sessionPassphrase = ''
    // #416 — read-receipt state is strictly per-message.
    mdnMode = null
    mdnBusy = false
    mdnError = ''
    receiptStatus = null
    // #341 — refresh the "Unlock automatically" mirror for the
    // account this message belongs to.  Done in parallel with the
    // cache + IMAP fetches below so the auto-decrypt attempt that
    // depends on it doesn't have to wait on an extra round-trip.
    void api.crypto.pgpHasUnlockAutomatically({ accountId: id })
      .then((on) => {
        if (id === accountId && f === folder && u === uid) {
          autoUnlockForAccount = on
        }
      })
      .catch(() => {
        // Best-effort — a keychain hiccup just means the toggle
        // visual lags by one message-open.  The IPC fallback in
        // the backend stays correct either way.
      })

    // Cache first — lets the reading pane paint instantly when the user
    // re-opens a previously read message (the common case).
    try {
      const cached = await api.mail.getCachedMessage({
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
      const fresh = await api.mail.fetchMessage({
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

    // #341 — auto-decrypt on open.  Calls `try_auto_decrypt_message`
    // unconditionally: the backend cheaply returns `Ok(None)` when
    // the account hasn't opted in (a single keychain query, no IMAP
    // round-trip), so opt-out users pay essentially nothing here.
    // Skip when the cached body already holds plaintext from an
    // earlier successful Decrypt — re-running the IPC would
    // re-fetch the raw bytes from IMAP for no gain.
    if (
      email &&
      id === accountId &&
      f === folder &&
      u === uid &&
      emailBodyLooksEncrypted(email)
    ) {
      try {
        const auto = await api.crypto.tryAutoDecryptMessage({
          accountId: id,
          folder: f,
          uid: u,
        })
        if (auto && id === accountId && f === folder && u === uid) {
          email = auto
          // No `sessionPassphrase` write here on purpose — the
          // backend resolved the passphrase via the keychain and
          // never handed it back to us.  Attachment fetches for
          // this same message will route through the keychain too
          // (the `autoUnlockForAccount` gate above lets them
          // bypass the empty-sessionPassphrase short-circuit).
        }
      } catch (e: any) {
        // Auto-decrypt failed (passphrase no longer unlocks, key
        // rotated, ciphertext corrupt, …).  Leave the manual
        // Decrypt button visible so the user can recover by
        // typing a fresh passphrase, and log so we can spot
        // pattern issues — but don't surface a banner here: the
        // existing protection chip + Decrypt UI already
        // communicates "this needs decrypting" clearly enough.
        console.warn('try_auto_decrypt_message failed:', e)
      }
    }

    // Mark as read — fire-and-forget. The MailList picked up an optimistic
    // cache update from the backend, and onread() lets the parent refresh
    // the envelope list so the unread styling clears immediately.
    if (email && !email.is_read && id === accountId && f === folder && u === uid) {
      try {
        await api.mail.markAsRead({ accountId: id, folder: f, uid: u })
        if (email) email.is_read = true
        onread?.(u)
      } catch (e: any) {
        console.warn('mark_as_read failed:', e)
      }
    }

    // #416 — read receipts, both directions, after the message is
    // confirmed displayed (this is the "displayed" moment RFC 8098's
    // disposition reports).
    if (email && id === accountId && f === folder && u === uid) {
      // Incoming request: resolve the user's never/ask/always policy.
      // `never` leaves mdnMode as-is (banner condition never true);
      // `ask` arms the banner; `always` fires the receipt silently.
      if (email.mdn_requested_to && !email.mdn_handled) {
        try {
          const settings = await api.settings.getAppSettings()
          if (id === accountId && f === folder && u === uid) {
            mdnMode = settings?.mdn_response_mode ?? 'ask'
            if (mdnMode === 'always') void respondMdn(false, true)
          }
        } catch (e: any) {
          // Settings unavailable — fall back to asking; sending
          // silently without a confirmed policy is the one wrong move.
          console.warn('get_app_settings failed (receipt banner):', e)
          mdnMode = 'ask'
        }
      }
      // Outgoing status: does this mail have a tracked receipt
      // request?  Only ever non-null for mail we sent with the
      // Compose toggle on, so the chip stays absent everywhere else.
      if (email.message_id) {
        try {
          const status = await api.mail.getReceiptStatus({
            accountId: id,
            messageId: email.message_id,
          })
          if (id === accountId && f === folder && u === uid) receiptStatus = status
        } catch (e: any) {
          console.warn('get_receipt_status failed:', e)
        }
      }
    }
  }

  // ── Read receipts (#416) ──────────────────────────────────────
  /** Response policy for the open message: `'ask'` arms the banner,
   *  `'always'` already fired, `'never'`/`null` render nothing. */
  let mdnMode = $state<string | null>(null)
  let mdnBusy = $state(false)
  let mdnError = $state('')
  /** Sent-mail receipt tracking for the open message, when it asked
   *  for one. */
  let receiptStatus = $state<SentReceiptStatus | null>(null)

  /** Send (or decline) the read receipt for the open message.
   *  `automatic` marks the receipt as policy-fired (`Always` mode)
   *  so the MDN itself reports `automatic-action` per RFC 8098. */
  async function respondMdn(decline: boolean, automatic = false) {
    if (!email || uid == null || mdnBusy) return
    mdnBusy = true
    mdnError = ''
    try {
      await api.mail.respondMdnRequest({ accountId, folder, uid, decline, automatic })
      // Mirror the backend's mdn_handled stamp so the banner drops
      // without a refetch.
      if (email) email.mdn_handled = decline ? 'declined' : 'sent'
    } catch (e: any) {
      console.warn('respond_mdn_request failed:', e)
      mdnError = formatError(e) || 'Failed to send the read receipt'
    } finally {
      mdnBusy = false
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
      await api.mail.setMessageRead({
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

  // ── Flag / pin / priority (#414) ──────────────────────────────
  // Same optimistic shape as toggleRead: flip the local state so
  // the button/badge updates instantly, invoke the backend, revert
  // on failure, and notify the parent so the MailList row follows.
  //
  // Display state prefers the live `listState` row over the
  // locally-fetched `email` copy — the list row is the one both
  // surfaces mutate optimistically, so rendering from it keeps the
  // toolbar in sync with toggles made in the mail list while this
  // message is open.

  const shownFlagged = $derived(listState?.is_starred ?? email?.is_starred ?? false)
  const shownPinned = $derived(listState?.is_pinned ?? email?.is_pinned ?? false)

  async function toggleFlagged() {
    if (!email || uid == null) return
    const next = !shownFlagged
    // Notify the parent BEFORE the backend call — the toolbar
    // renders from the list row, and the flag IPC includes an IMAP
    // round-trip, so waiting for it would visibly lag the button.
    email.is_starred = next
    onflagchanged?.(uid, next)
    try {
      await api.mail.setMessageFlagged({ accountId, folder, uid, flagged: next })
    } catch (e: any) {
      console.warn('set_message_flagged failed:', e)
      if (email) email.is_starred = !next
      onflagchanged?.(uid, !next)
    }
  }

  async function togglePinned() {
    if (!email || uid == null) return
    const next = !shownPinned
    email.is_pinned = next
    onpinchanged?.(uid, next)
    try {
      await api.mail.setMessagePinned({ accountId, folder, uid, pinned: next })
    } catch (e: any) {
      console.warn('set_message_pinned failed:', e)
      if (email) email.is_pinned = !next
      onpinchanged?.(uid, !next)
    }
  }

  /** Popover for the priority picker.  Uses the project's standard
   *  outside-click dismissal idiom: the document listener is
   *  registered one tick after open so the click that opened the
   *  menu doesn't immediately dismiss it. */
  let priorityMenuOpen = $state(false)
  $effect(() => {
    if (!priorityMenuOpen) return
    const close = () => (priorityMenuOpen = false)
    const t = setTimeout(() => document.addEventListener('mousedown', close), 0)
    return () => {
      clearTimeout(t)
      document.removeEventListener('mousedown', close)
    }
  })

  /** The priority the badge/menu should reflect: user override
   *  first, then the sender-declared header value; `'normal'`
   *  renders no badge.  Reads the whole tuple from `listState`
   *  when present (never mixes the two sources — a cleared
   *  override in the list must not fall back to a stale one on
   *  the fetched copy). */
  function effectivePriority(): 'high' | 'low' | null {
    const src = listState ?? email
    const p = src?.priority_override ?? src?.priority
    return p === 'high' || p === 'low' ? p : null
  }

  async function setPriority(priority: 'high' | 'normal' | 'low') {
    if (!email || uid == null) return
    const prev = (listState ?? email)?.priority_override ?? null
    email.priority_override = priority
    onprioritychanged?.(uid, priority)
    priorityMenuOpen = false
    try {
      await api.mail.setMessagePriority({ accountId, folder, uid, priority })
    } catch (e: any) {
      console.warn('set_message_priority failed:', e)
      if (email) email.priority_override = prev
      onprioritychanged?.(uid, prev)
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
  //   • marks cid: anchors with data-unkai-cid for the click handler
  //   • unless showImages is true, replaces every remote <img src> with
  //     a transparent 1×1 GIF and stashes the original in
  //     data-unkai-blocked-src

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

  // ── Collapse quoted / forwarded blocks (#330) ────────────────────────
  //
  // Display-only transformation: every standard "this is a quoted /
  // forwarded chunk" marker the wild collection of mail clients
  // emits is folded into a native <details>/<summary> disclosure
  // card so the reader sees the user's fresh content first and can
  // expand the previous conversation on demand.  The raw
  // `email.body_html` is never touched — Reply / Forward continue
  // to read the unmodified body and emit the same wire format every
  // other mail client expects.
  //
  // Markers handled, most-specific first.  Each pass skips elements
  // already inside a `.quoted-collapse` to avoid double-wrapping
  // nested matches (e.g. a `<blockquote type="cite">` inside our
  // own `data-unkai-block="quoted-history"`).
  //
  //   1. `div[data-unkai-block="forwarded-mail"]` — our own forward
  //      wrapper.  Parses the embedded From / Date / Subject / To
  //      lines into a styled mini-header card; the body lands
  //      below.
  //   2. `div[data-unkai-block="quoted-history"]` — our own reply
  //      wrapper.  Collapsed without re-styling — the inner
  //      blockquote already has the muted-grey treatment.
  //   3. `blockquote[type="cite"]` — the cite-blockquote
  //      convention used by Apple Mail, Thunderbird, and the
  //      RFC-3676 reply quoting style.
  //   4. `div.gmail_quote`, `div.gmail_quote_container` — the
  //      class names the dominant webmail emits on its reply
  //      blocks.
  //   5. `div.moz-cite-prefix` + the immediately-following
  //      `<blockquote>` — Thunderbird splits the citation line
  //      from the quoted body across two siblings; we wrap them
  //      together.
  //   6. The plain-text delimiter `---------- Forwarded message
  //      ----------` followed by the rest of the body.  This is
  //      the de-facto convention used when the sender's client
  //      doesn't wrap forwards in a class-bearing div (and it's
  //      what our own outgoing forwards emit, on top of the
  //      data-unkai-block wrapper, so a receiving client without
  //      the wrapper still recognises the chunk).
  //
  // The styled mini-header for case (1) is intentionally scoped to
  // markers we own.  Universal multi-format parsing across every
  // client's idiosyncratic header shape (Apple Mail's single
  // line, Gmail's "On X at Y, Z wrote:", Thunderbird's table) is
  // its own feature; for v1 the non-Unkai cases just collapse
  // without restyling.

  /** Does this element carry any visible content?  We treat empty
   *  `<blockquote>` / `<div>` shells (common as line-spacers in mail
   *  forwarded between clients) as "no content" so we don't render
   *  empty disclosure cards over them.  Whitespace-only text doesn't
   *  count; an `<img>` / `<video>` etc. does. */
  function hasVisibleContent(el: Element): boolean {
    if ((el.textContent ?? '').trim().length > 0) return true
    return el.querySelector('img, video, audio, picture, canvas, svg') !== null
  }

  // Disclosure summary labels by chunk kind.  Replies show
  // "conversation history"; forwards show "forwarded messages".
  // For the ambiguous `<blockquote type="cite">` / `gmail_quote`
  // markers (both clients use the same wrapper for replies AND
  // forwards) we sniff the chunk text against forward-preamble
  // markers below to pick the right label.
  const REPLY_SUMMARY = 'Show conversation history'
  const FORWARD_SUMMARY = 'Show forwarded messages'

  // Multilingual forward-preamble markers used both to label
  // ambiguous chunks as forwards and to recognise forward
  // metadata in the body's outside-text for the auto-open
  // heuristic.  Kept broad on purpose because different mail
  // clients localise the preamble line and split the field
  // labels differently.
  const FORWARD_PREAMBLE_RE =
    /(?:begin\s+forwarded\s+message|forwarded\s+message|anfang\s+der\s+weitergeleiteten\s+nachricht|weitergeleitete\s+nachricht|mensaje\s+reenviado|message\s+transféré|messaggio\s+inoltrato)/i
  const FROM_LABEL_RE = /\b(?:from|von|de|fra|od):/i
  const DATE_LABEL_RE =
    /\b(?:date|datum|sent|fecha|envoyé|data|inviato|gesendet):/i
  const SUBJECT_LABEL_RE =
    /\b(?:subject|betreff|asunto|objet|oggetto):/i

  /** Looks at the chunk's plain text and decides whether it's a
   *  forward (preamble marker, or a From: + Date:/Subject: header
   *  cluster) or a reply (default).  Used to pick the disclosure
   *  summary label for the ambiguous wrapper markers — Apple Mail
   *  / Thunderbird `<blockquote type="cite">` and Gmail's
   *  `gmail_quote` both wrap replies AND forwards. */
  function classifyChunk(text: string): 'forward' | 'reply' {
    if (FORWARD_PREAMBLE_RE.test(text)) return 'forward'
    if (
      FROM_LABEL_RE.test(text) &&
      (DATE_LABEL_RE.test(text) || SUBJECT_LABEL_RE.test(text))
    ) {
      return 'forward'
    }
    return 'reply'
  }

  function summaryFor(text: string): string {
    return classifyChunk(text) === 'forward' ? FORWARD_SUMMARY : REPLY_SUMMARY
  }

  /** Build a native disclosure card around the given inner HTML.
   *  The `<details>` element is closed by default, click on
   *  `<summary>` toggles.  We mark the wrapper with a class so the
   *  outer click handler (which delegates anchor / attachment-ref
   *  clicks) can recognise descendants and skip the summary
   *  toggle path entirely — and so the nested-collapse pass
   *  skips elements already inside a wrapped block. */
  function buildCollapseCard(
    doc: Document,
    summaryText: string,
    headerHtml: string,
    bodyHtml: string,
  ): HTMLDetailsElement {
    const details = doc.createElement('details')
    details.className = 'quoted-collapse'
    const summary = doc.createElement('summary')
    summary.className = 'quoted-collapse__summary'
    summary.textContent = summaryText
    details.appendChild(summary)
    const content = doc.createElement('div')
    content.className = 'quoted-collapse__content'
    if (headerHtml) {
      const header = doc.createElement('div')
      header.className = 'quoted-collapse__header'
      header.innerHTML = headerHtml
      content.appendChild(header)
    }
    const body = doc.createElement('div')
    body.className = 'quoted-collapse__body'
    body.innerHTML = bodyHtml
    content.appendChild(body)
    details.appendChild(content)
    return details
  }

  /** For our own `data-unkai-block="forwarded-mail"` wrappers,
   *  pull the embedded From / Date / Subject / To rows out of the
   *  source HTML and rebuild them as a styled key-value list.
   *  Everything *after* those rows is treated as the forwarded
   *  body.  The shape we emit (see `forwardedMailHtml` in
   *  `inviteHtml.ts`) is:
   *
   *      <p>---------- Forwarded message ----------</p>
   *      <div><b>From:</b> …</div>
   *      <div><b>Date:</b> …</div>
   *      <div><b>Subject:</b> …</div>
   *      <div><b>To:</b> …</div>
   *      [body]
   *
   *  After DOMPurify the structure round-trips intact, so this
   *  parser is a direct DOM walk.  Defensive fallback: if no
   *  header rows are found (e.g. an older format we predate),
   *  return the whole interior as body with no styled header. */
  function parseOurForwardedHeaders(
    el: Element,
  ): { headerHtml: string; bodyHtml: string } {
    const esc = (s: string) =>
      s
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
    interface Field {
      label: string
      value: string
    }
    const fields: Field[] = []
    const LABEL_RE = /^(From|Date|Subject|To|Cc):\s*/i
    let bodyStartIdx = -1
    const kids = Array.from(el.children)
    for (let i = 0; i < kids.length; i++) {
      const child = kids[i]
      const text = (child.textContent ?? '').trim()
      // Skip the literal "---------- Forwarded message ----------" line.
      if (/^-{5,}\s*forwarded message\s*-{5,}$/i.test(text)) continue
      const m = text.match(LABEL_RE)
      if (m && child.querySelector('b')) {
        fields.push({
          label: m[1],
          value: text.slice(m[0].length),
        })
        continue
      }
      // First child that isn't a header row marks the body start.
      bodyStartIdx = i
      break
    }
    if (fields.length === 0) {
      return { headerHtml: '', bodyHtml: el.innerHTML }
    }
    const headerHtml = fields
      .map(
        (f) =>
          `<div class="quoted-collapse__field"><span class="quoted-collapse__label">${esc(f.label)}</span><span class="quoted-collapse__value">${esc(f.value)}</span></div>`,
      )
      .join('')
    let bodyHtml = ''
    if (bodyStartIdx !== -1) {
      const wrapper = el.ownerDocument.createElement('div')
      for (let i = bodyStartIdx; i < kids.length; i++) {
        wrapper.appendChild(kids[i].cloneNode(true))
      }
      bodyHtml = wrapper.innerHTML
    }
    return { headerHtml, bodyHtml }
  }

  /** Find the text node containing the `---------- Forwarded
   *  message ----------` delimiter (if any) and return both the
   *  delimiter node and its containing block element (the element
   *  that's a direct child of `<body>`).  Returns `null` when no
   *  delimiter is present, or when it's already inside a
   *  `.quoted-collapse` wrapper from an earlier pass. */
  function findForwardedDelimiter(
    doc: Document,
  ): { containerBlock: Element } | null {
    const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT)
    let node: Node | null
    while ((node = walker.nextNode())) {
      const text = node.textContent ?? ''
      if (!/-{5,}\s*forwarded message\s*-{5,}/i.test(text)) continue
      const el = node.parentElement
      if (!el) continue
      if (el.closest('.quoted-collapse')) return null
      // Walk up to the direct child of <body> so we wrap from the
      // delimiter's block, not just its inline <strong> or <span>.
      let block: Element = el
      while (block.parentElement && block.parentElement !== doc.body) {
        block = block.parentElement
      }
      return { containerBlock: block }
    }
    return null
  }

  /** Greedy run-collector: take a starting element and walk its
   *  next siblings, absorbing any further matches plus filler
   *  (whitespace text nodes, empty elements like `<br>` or empty
   *  `<p>`).  Stops at the first sibling that is meaningful but
   *  isn't a match — that's the boundary between this quoted run
   *  and whatever follows it.  Returns the collected nodes; the
   *  starting element is always first.
   *
   *  Used to coalesce mail clients that split a single logical
   *  forwarded chunk across multiple sibling elements (e.g. Apple
   *  Mail emits one `<blockquote type="cite">` for the embedded
   *  headers and a second for the body — without this we'd render
   *  them as two adjacent disclosure cards). */
  function collectMatchRun(
    start: Element,
    isMatch: (el: Element) => boolean,
  ): Node[] {
    const run: Node[] = [start]
    let cursor: Node | null = start.nextSibling
    while (cursor) {
      if (cursor.nodeType === Node.TEXT_NODE) {
        if ((cursor.textContent ?? '').trim().length === 0) {
          run.push(cursor)
          cursor = cursor.nextSibling
          continue
        }
        break // non-whitespace text breaks the run
      }
      if (cursor.nodeType !== Node.ELEMENT_NODE) {
        cursor = cursor.nextSibling
        continue
      }
      const el = cursor as Element
      if (isMatch(el) || !hasVisibleContent(el)) {
        run.push(el)
        cursor = cursor.nextSibling
        continue
      }
      break // meaningful non-match content ends the run
    }
    return run
  }

  /** Wrap a run of nodes (returned by `collectMatchRun`) in a
   *  single collapse card, replacing the first node and detaching
   *  the rest. */
  function wrapRun(
    doc: Document,
    run: Node[],
    summaryText: string,
    headerHtml: string,
  ): void {
    const wrapper = doc.createElement('div')
    for (const node of run) wrapper.appendChild(node.cloneNode(true))
    const card = buildCollapseCard(doc, summaryText, headerHtml, wrapper.innerHTML)
    const [first, ...rest] = run as [Element, ...Node[]]
    first.replaceWith(card)
    for (const node of rest) {
      if (node.parentNode) node.parentNode.removeChild(node)
    }
  }

  function collapseQuotedBlocks(doc: Document) {
    // Pass 1 — our own forwarded-mail wrapper (with mini-header).
    doc
      .querySelectorAll('div[data-unkai-block="forwarded-mail"]')
      .forEach((el) => {
        if (el.closest('.quoted-collapse')) return
        if (!hasVisibleContent(el)) return
        const { headerHtml, bodyHtml } = parseOurForwardedHeaders(el)
        el.replaceWith(
          buildCollapseCard(doc, FORWARD_SUMMARY, headerHtml, bodyHtml),
        )
      })

    // Pass 2 — our own quoted-history (reply) wrapper.
    doc
      .querySelectorAll('div[data-unkai-block="quoted-history"]')
      .forEach((el) => {
        if (el.closest('.quoted-collapse')) return
        if (!hasVisibleContent(el)) return
        el.replaceWith(
          buildCollapseCard(doc, REPLY_SUMMARY, '', el.innerHTML),
        )
      })

    // Pass 3 — standard cite-blockquote.  Coalesces adjacent
    // siblings into one card so the embedded-header blockquote and
    // the body blockquote a forward gets split into render as a
    // single card rather than two.  Summary label is chosen by
    // sniffing the merged chunk for forward-preamble markers —
    // the same `<blockquote type="cite">` wrapper carries both
    // reply quotes and forwarded chunks across mail clients.
    const isCiteBq = (el: Element) =>
      el.tagName.toLowerCase() === 'blockquote' &&
      el.getAttribute('type')?.toLowerCase() === 'cite'
    for (const bq of Array.from(doc.querySelectorAll('blockquote[type="cite"]'))) {
      if (!bq.isConnected) continue // already absorbed into an earlier run
      if (bq.closest('.quoted-collapse')) continue
      if (!hasVisibleContent(bq)) continue
      const run = collectMatchRun(bq, isCiteBq)
      const runText = run
        .map((n) => n.textContent ?? '')
        .join(' ')
        .replace(/\s+/g, ' ')
        .trim()
      wrapRun(doc, run, summaryFor(runText), '')
    }

    // Pass 4 — webmail "gmail_quote" / "gmail_quote_container" divs.
    const isGmailQuote = (el: Element) =>
      el.tagName.toLowerCase() === 'div' &&
      (el.classList.contains('gmail_quote') ||
        el.classList.contains('gmail_quote_container'))
    for (const el of Array.from(
      doc.querySelectorAll('div.gmail_quote, div.gmail_quote_container'),
    )) {
      if (!el.isConnected) continue
      if (el.closest('.quoted-collapse')) continue
      if (!hasVisibleContent(el)) continue
      const run = collectMatchRun(el, isGmailQuote)
      const runText = run
        .map((n) => n.textContent ?? '')
        .join(' ')
        .replace(/\s+/g, ' ')
        .trim()
      wrapRun(doc, run, summaryFor(runText), '')
    }

    // Pass 5 — moz-cite-prefix + following blockquote (paired).
    doc.querySelectorAll('div.moz-cite-prefix').forEach((prefix) => {
      if (prefix.closest('.quoted-collapse')) return
      const next = prefix.nextElementSibling
      if (!next || next.tagName.toLowerCase() !== 'blockquote') return
      if (!hasVisibleContent(prefix) && !hasVisibleContent(next)) return
      const wrapper = doc.createElement('div')
      wrapper.appendChild(prefix.cloneNode(true))
      wrapper.appendChild(next.cloneNode(true))
      const runText = ((prefix.textContent ?? '') + ' ' + (next.textContent ?? ''))
        .replace(/\s+/g, ' ')
        .trim()
      const card = buildCollapseCard(
        doc,
        summaryFor(runText),
        '',
        wrapper.innerHTML,
      )
      next.remove()
      prefix.replaceWith(card)
    })

    // Pass 6 — plain-text "---------- Forwarded message ----------"
    // delimiter.  Wraps the delimiter block plus every sibling
    // that follows it, so a forward without a wrapping div still
    // collapses.  Runs last so the structured-wrapper passes
    // above (which produce a `.quoted-collapse` ancestor) get
    // first dibs on the delimiter and we don't double-wrap.  By
    // construction this is always a forward.
    const hit = findForwardedDelimiter(doc)
    if (hit) {
      const wrapper = doc.createElement('div')
      let cursor: Element | null = hit.containerBlock
      const toMove: Element[] = []
      while (cursor) {
        toMove.push(cursor)
        cursor = cursor.nextElementSibling
      }
      for (const el of toMove) wrapper.appendChild(el)
      if (hasVisibleContent(wrapper)) {
        const card = buildCollapseCard(
          doc,
          FORWARD_SUMMARY,
          '',
          wrapper.innerHTML,
        )
        doc.body.appendChild(card)
      }
    }

    // Pure-forward / pure-quote case: when the body's *outside*
    // content (everything not in a collapse card) is either tiny,
    // looks like forward preamble metadata, or is overwhelmed by
    // the collapsed content size-wise, the user's primary interest
    // IS the collapsed chunk (a mail forwarded to them, a thread
    // where they have no fresh reply on top, etc).  Open every
    // collapse by default; the toggle is still there if they want
    // to fold it away.
    //
    // Three OR'd conditions, in increasing leniency:
    //
    //   1. outside text < 60 chars — clearly a pure forward with
    //      nothing else around it.
    //   2. outside text looks like forward preamble — detected via
    //      multilingual marker strings ("Begin forwarded message",
    //      "Anfang der weitergeleiteten Nachricht", From: + Date:
    //      / Subject: cluster) — and is bounded in length so a
    //      reply that happens to quote those words in passing
    //      doesn't false-positive.
    //   3. collapsed content is ≥ 85% of total body text — the
    //      catch-all for very large forwarded chunks (newsletters,
    //      long signatures) where the outside metadata bumps past
    //      the preamble check but is still negligible relative to
    //      the actual content.
    const allCollapses = Array.from(
      doc.querySelectorAll('details.quoted-collapse'),
    ) as HTMLDetailsElement[]
    if (allCollapses.length > 0) {
      const clone = doc.body.cloneNode(true) as HTMLElement
      clone
        .querySelectorAll('details.quoted-collapse')
        .forEach((d) => d.remove())
      const outsideText = (clone.textContent ?? '').replace(/\s+/g, ' ').trim()
      const insideText = allCollapses.reduce(
        (sum, d) =>
          sum + (d.textContent ?? '').replace(/\s+/g, ' ').trim().length,
        0,
      )
      const totalText = outsideText.length + insideText

      // Reuses the same `classifyChunk` heuristic that picks the
      // summary label, so any text outside the collapses that
      // looks like forward preamble doesn't block auto-open.
      const looksLikePreamble = classifyChunk(outsideText) === 'forward'

      const shouldOpen =
        outsideText.length < 60 ||
        (outsideText.length < 500 && looksLikePreamble) ||
        (totalText > 100 && insideText / totalText >= 0.85)

      if (shouldOpen) {
        for (const d of allCollapses) d.setAttribute('open', '')
      }
    }
  }

  function processEmailHtml(
    html: string,
    showImages: boolean,
    inlineUrls: Record<string, string>,
    inlineLoading: boolean,
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
          'data-unkai-cid',
          'data-unkai-blocked-src',
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
          a.setAttribute('data-unkai-cid', cid)
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
            img.setAttribute('data-unkai-blocked-src', src)
            img.setAttribute('src', BLOCKED_IMG_PLACEHOLDER)
            img.removeAttribute('srcset')
            const alt = img.getAttribute('alt') ?? ''
            if (!alt) img.setAttribute('alt', '(blocked image)')
            img.setAttribute('title', 'Remote image blocked — click "Show images" to load')
          }
        })
      }

      // Resolve `cid:` sources against the message's own image parts
      // (#471).  Runs after the remote-image pass — which skips
      // `cid:` deliberately, since these bytes are already in the
      // message and need no "Show images" consent — and before the
      // quote fold, so an inline image inside a quoted chunk is
      // resolved too.
      applyInlineImages(doc, inlineUrls, inlineLoading)

      // Fold quoted / forwarded chunks into collapsible cards (#330).
      // Runs after the link / image passes so anything inside a
      // collapsed block has already been annotated and image-blocked;
      // the URLhaus second-pass that walks `<a href>` later will see
      // the links inside the (still-in-DOM, just-hidden) block too.
      collapseQuotedBlocks(doc)

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
        inlineImageUrls,
        inlineImagesLoading,
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
        inlineImageUrls,
        inlineImagesLoading,
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
    void api.mail.checkUrls({ urls })
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
        pill.setAttribute('data-unkai-link-pill', v.verdict)
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
          a.setAttribute('data-unkai-unsafe-link', '1')
          if (v.threat) a.setAttribute('data-unkai-threat', v.threat)
          if (v.exact) a.setAttribute('data-unkai-link-exact', '1')
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
      await api.system.openUrl({ url })
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

    // Tiptap-rendered attachment refs from Unkai (#93).
    // Two on-the-wire shapes float around:
    //   - new: <span data-attachment-ref data-cid=... data-filename=...>
    //   - legacy: <a href="cid:..." data-attachment-ref>
    // Plus an intermediate where DOMPurify's cid: handler has
    // already moved the cid into `data-unkai-cid`.  We resolve
    // through every channel so a click works regardless of the
    // sending client's age or what survived the round-trip.
    const refEl = target.closest('[data-attachment-ref]') as HTMLElement | null
    if (refEl) {
      e.preventDefault()
      e.stopPropagation()
      if (!email) return
      // CID resolution: explicit data-cid → data-unkai-cid
      // (set by processEmailHtml on legacy anchors) → href.
      let cidAttr = (refEl.getAttribute('data-cid') ?? '').trim()
      if (!cidAttr) cidAttr = (refEl.getAttribute('data-unkai-cid') ?? '').trim()
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

    const cid = anchor.getAttribute('data-unkai-cid')
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
    // unhelpful when Unkai *is* the user's mail client).  RFC 6068
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
    // anchor is tagged with `data-unkai-unsafe-link` by
    // `annotateLinkPills` only when the verdict came back
    // 'unsafe' (and the master toggle is on), so a missing
    // attribute is the safe / off path that keeps the original
    // open-in-browser behaviour.
    if (anchor.hasAttribute('data-unkai-unsafe-link')) {
      e.preventDefault()
      e.stopPropagation()
      unsafeLinkPrompt = {
        url: href,
        threat: anchor.getAttribute('data-unkai-threat'),
        exact: anchor.hasAttribute('data-unkai-link-exact'),
      }
      return
    }
    e.preventDefault()
    void api.system.openUrl({ url: href })
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
    setBusy(att.part_id, true)
    try {
      // #341 — route through `fetchAttachmentBytes` so encrypted
      // messages pull bytes from the decrypted inner MIME tree
      // rather than the outer envelope's `application/pgp-encrypted`
      // "Version: 1" header part.
      await openAttachment(att, () => fetchAttachmentBytes(att))
    } catch (e) {
      error = formatAttachmentFetchError(e, 'Failed to open attachment')
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
   *    filename) via `api.platform.saveFileDialog`.
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
      chosenPath = await api.platform.saveFileDialog({
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
      const bytes = await fetchAttachmentBytes(att)
      await api.system.saveBytesToPath({ path: chosenPath, data: bytes })
    } catch (e) {
      error = formatAttachmentFetchError(e, 'Failed to download attachment')
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
      const bytes = await fetchAttachmentBytes(att)
      await api.system.printAttachment({
        fileName: att.filename,
        bytes,
      })
    } catch (e) {
      error = formatAttachmentFetchError(e, 'Failed to print attachment')
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
      const bytes = await fetchAttachmentBytes(att)
      // Join the folder path with the filename, avoiding double slashes
      // when folderPath is just '/'.
      const base = folderPath.endsWith('/') ? folderPath : `${folderPath}/`
      const target = `${base}${att.filename}`
      await api.nextcloud.uploadToNextcloud({
        ncId,
        path: target,
        data: bytes,
        contentType: att.content_type || null,
      })
    } catch (e) {
      error = formatAttachmentFetchError(e, 'Failed to save to Nextcloud')
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
      await api.mail.moveMessage({
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
        await api.mail.archiveMessages({
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
      await api.mail.archiveMessage({
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
      await api.mail.deleteMessage({
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
      <!-- `flex-wrap` (#454): on a narrow pane the badge/date cluster
           drops below the subject instead of clipping past the
           pane's right edge (the cluster is shrink-0 by design so
           the date never squashes). -->
      <div class="flex flex-wrap items-start justify-between mb-2 gap-x-4 gap-y-1">
        <h2 class="text-xl font-semibold flex items-center gap-2 min-w-0">
          {#if shownPinned}
            <span
              class="shrink-0 inline-flex items-center text-primary-500"
              title={m.mail_pinned_title()}
              aria-label={m.mail_pinned_title()}
            ><Icon name="pin" size={18} /></span>
          {/if}
          <span class="min-w-0">{email.subject || '(no subject)'}</span>
        </h2>
        <div class="flex items-center gap-3 shrink-0">
          <!-- Priority badge (#414): the user's override wins over
               the sender-declared header value; normal shows
               nothing. -->
          {#if effectivePriority() === 'high'}
            <span
              class="inline-flex items-center gap-1 text-xs leading-none px-2 py-1 rounded-full bg-red-500/15 text-red-600 dark:text-red-400"
              title={m.mail_priority_high_badge()}
            >
              <Icon name="important" size={12} />
              <span class="font-medium">{m.mail_priority_high()}</span>
            </span>
          {:else if effectivePriority() === 'low'}
            <span
              class="inline-flex items-center gap-1 text-xs leading-none px-2 py-1 rounded-full bg-surface-200 text-surface-600 dark:bg-surface-700 dark:text-surface-300"
              title={m.mail_priority_low_badge()}
            >
              <span class="font-medium">{m.mail_priority_low()}</span>
            </span>
          {/if}
          <!-- #416 — sent-mail receipt status.  Only ever present for
               mail this client sent with "request read receipt" on
               (the status lookup is keyed on tracked Message-IDs).
               Green once a "displayed" report came back; neutral
               while pending — and the pending tooltip manages
               expectations, since recipients can decline and the
               receipt may simply never arrive. -->
          {#if receiptStatus}
            {#if receiptStatus.disposition === 'displayed'}
              <span
                class="inline-flex items-center gap-1 text-xs leading-none px-2 py-1 rounded-full bg-success-500/15 text-success-600 dark:text-success-400"
                title={receiptStatus.reporter ?? ''}
              >
                <Icon name="read" size={12} />
                <span class="font-medium">
                  {m.mail_view_receipt_read({
                    date: formatFullDate(
                      new Date(
                        (receiptStatus.disposition_at ?? receiptStatus.requested_at) * 1000,
                      ).toISOString(),
                    ),
                  })}
                </span>
              </span>
            {:else if receiptStatus.disposition == null}
              <span
                class="inline-flex items-center gap-1 text-xs leading-none px-2 py-1 rounded-full bg-surface-200 text-surface-600 dark:bg-surface-700 dark:text-surface-300"
                title={m.mail_view_receipt_requested_title()}
              >
                <Icon name="read" size={12} />
                <span class="font-medium">{m.mail_view_receipt_requested()}</span>
              </span>
            {:else}
              <!-- Any non-"displayed" disposition (deleted, processed,
                   …) — report receipt without claiming it was read. -->
              <span
                class="inline-flex items-center gap-1 text-xs leading-none px-2 py-1 rounded-full bg-surface-200 text-surface-600 dark:bg-surface-700 dark:text-surface-300"
                title={receiptStatus.disposition}
              >
                <Icon name="read" size={12} />
                <span class="font-medium">{m.mail_view_receipt_received()}</span>
              </span>
            {/if}
          {/if}
          <span class="text-sm text-surface-500">{formatFullDate(email.date)}</span>
        </div>
      </div>
      <div class="flex items-center gap-2 text-sm text-surface-600 dark:text-surface-400">
        <span class="font-medium">{email.from || '(unknown sender)'}</span>
      </div>
      <!-- #57 — encryption + signature status chips and the
           "can't decrypt here" banner.  Component renders nothing
           when both fields are null, which is the common-case
           plaintext path. -->
      <CryptoChips
        protection={email.protection}
        signatureStatus={email.signature_status}
        signerFingerprint={email.signer_fingerprint}
        decrypted={!!(email.body_text || email.body_html)}
      />
      {#if (email.protection === 'encrypted' || email.protection === 'signed-and-encrypted')
        && !email.body_text && !email.body_html}
        <!-- #57 — Decrypt prompt.  Appears when the receive path
             marked the message encrypted but body is empty (no
             bridge was supplied on first fetch, by design — we
             re-prompt for the passphrase on every decrypt).  Two-
             row layout: explanation line at the top, then the
             input + button on a second row so the user reads the
             "what's going on" copy before reaching for the field. -->
        <div
          class="mt-2 rounded-lg border border-primary-300 bg-primary-50 dark:border-primary-700 dark:bg-primary-900/30 p-3 flex flex-col gap-2"
        >
          <!-- Explanation copy uses the same muted surface tone as
               the From-line in the header so the badge reads as
               *secondary information* rather than competing with
               the subject for attention. -->
          <p class="text-sm text-surface-600 dark:text-surface-400">
            This message is encrypted with your OpenPGP key.  Enter your
            passphrase to decrypt and view it.
          </p>
          <div class="flex items-center gap-2">
            <input
              type="password"
              class="input text-sm px-2 py-1.5 rounded-lg flex-1"
              placeholder="PGP passphrase"
              bind:value={decryptPassphrase}
              disabled={decrypting}
              autocomplete="off"
              onkeydown={(e) => {
                if (e.key === 'Enter' && decryptPassphrase) {
                  void runDecrypt()
                }
              }}
            />
            <!-- Hover keeps the resting outlined-surface look and
                 just brightens the border to the same lightest-
                 surface tone the input field uses when focused —
                 no fill, no text tint.  Any hue we tried (primary
                 or success) competed with the surrounding badge;
                 a neutral border bump signals "interactive" without
                 introducing a colour. -->
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center gap-1.5 hover:border-surface-50 shrink-0"
              disabled={decrypting || !decryptPassphrase}
              onclick={() => void runDecrypt()}
              title="Decrypt this message"
            >
              {decrypting ? 'Decrypting…' : 'Decrypt'}
            </button>
          </div>
        </div>
        {#if decryptError}
          <div class="mt-1 text-xs text-error-500" data-test="decrypt-error">
            {decryptError}
          </div>
        {/if}
      {/if}
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
    <!-- `flex-wrap` (#454): the reading pane absorbs all the shrink
         when the window narrows (the sidebar + list columns are
         fixed-width), and the app shell clips overflow — without
         wrapping, the right-side buttons (move / archive / delete)
         silently vanish with no way to scroll them back. Wrapping
         keeps every action visible on a second row instead. -->
    <div class="flex flex-wrap items-center gap-2 px-6 py-2 border-b glass-panel text-sm">
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
        {#if oncreatetask}
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
            onclick={() => email && uid != null && oncreatetask?.({ ...email, uid })}
            title="Create a Nextcloud task seeded from this email"
            aria-label="Create task from this email"
          ><Icon name="tasks" size={16} /></button>
        {/if}
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40"
          onclick={toggleRead}
          title={email.is_read ? 'Mark this message as unread' : 'Mark this message as read'}
          aria-label={email.is_read ? 'Mark as unread' : 'Mark as read'}
        ><Icon name={email.is_read ? 'unread' : 'read'} size={16} /></button>
        <!-- Flag / pin / priority (#414).  Flag + pin keep their
             icon lit while active so the toolbar doubles as the
             state indicator; priority opens a three-option
             popover anchored to the button. -->
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-amber-500/15 hover:text-amber-500 hover:border-amber-500/40 {shownFlagged ? 'text-amber-500 border-amber-500/40' : ''}"
          onclick={toggleFlagged}
          title={shownFlagged ? m.mail_action_unflag() : m.mail_action_flag()}
          aria-label={shownFlagged ? m.mail_action_unflag() : m.mail_action_flag()}
        ><Icon name="flag" size={16} /></button>
        <button
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40 {shownPinned ? 'text-primary-500 border-primary-500/40' : ''}"
          onclick={togglePinned}
          title={shownPinned ? m.mail_action_unpin() : m.mail_action_pin()}
          aria-label={shownPinned ? m.mail_action_unpin() : m.mail_action_pin()}
        ><Icon name="pin" size={16} /></button>
        <div class="relative">
          <button
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-primary-500/15 hover:text-primary-500 hover:border-primary-500/40 {effectivePriority() === 'high' ? 'text-red-500 border-red-500/40' : ''}"
            onclick={() => (priorityMenuOpen = !priorityMenuOpen)}
            title={m.mail_priority_label()}
            aria-label={m.mail_priority_label()}
          ><Icon name="important" size={16} /></button>
          {#if priorityMenuOpen}
            <!-- Stop mousedown from reaching the document-level
                 dismiss listener so the option handlers get to
                 run before the menu unmounts. -->
            <div
              class="absolute right-0 top-full mt-1 z-50 min-w-32 py-1 rounded-xl glass-float text-sm"
              role="menu"
              tabindex="-1"
              onmousedown={(e) => e.stopPropagation()}
            >
              {#each [
                { value: 'high' as const, label: m.mail_priority_high() },
                { value: 'normal' as const, label: m.mail_priority_normal() },
                { value: 'low' as const, label: m.mail_priority_low() },
              ] as opt (opt.value)}
                {@const active = (effectivePriority() ?? 'normal') === opt.value}
                <button
                  type="button"
                  class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 {active ? 'text-primary-500 font-medium' : ''}"
                  onclick={() => void setPriority(opt.value)}
                >
                  {#if active}<Icon name="success" size={14} />{:else}<span class="w-3.5"></span>{/if}
                  <span>{opt.label}</span>
                </button>
              {/each}
            </div>
          {/if}
        </div>
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
            <li class="relative flex items-center gap-2 pl-3 pr-1 py-1.5 rounded-lg bg-surface-100 dark:bg-surface-800 text-sm">
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
                    : () => fetchAttachmentBytes(att)}
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
                  class="absolute right-0 top-full mt-1 z-50 min-w-52 rounded-xl glass-float py-1 text-sm"
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

    <!-- #416 — read-receipt request banner (Ask mode).  Same amber
         advisory shape as the remote-images banner, but rendered
         outside the HTML-only branch because the request rides on
         plain-text mail just as often.  Disappears the moment the
         user picks either side (the response stamps `mdn_handled`),
         and never renders under the Never / Always policies. -->
    {#if email.mdn_requested_to && !email.mdn_handled && mdnMode === 'ask'}
      <div
        class="flex flex-wrap items-center gap-3 px-6 py-2 bg-amber-50 dark:bg-amber-900/20 border-b border-amber-200 dark:border-amber-700 text-sm text-amber-800 dark:text-amber-300"
      >
        <span class="shrink-0 inline-flex items-center gap-2">
          <Icon name="read" size={18} />
          {m.mail_view_mdn_banner({ sender: getSenderAddress(email.from) })}
        </span>
        <button
          class="btn btn-sm preset-outlined-surface-500"
          disabled={mdnBusy}
          onclick={() => void respondMdn(false)}
        >{mdnBusy ? m.mail_view_mdn_sending() : m.mail_view_mdn_send()}</button>
        <button
          class="btn btn-sm preset-outlined-surface-500"
          disabled={mdnBusy}
          onclick={() => void respondMdn(true)}
        >{m.mail_view_mdn_decline()}</button>
        {#if mdnError}
          <span class="text-xs text-error-500">{mdnError}</span>
        {/if}
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
      <div class="glass-float rounded-2xl w-md max-w-full mx-4 p-6 space-y-4">
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

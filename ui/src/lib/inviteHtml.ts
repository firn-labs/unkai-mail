/**
 * inviteHtml — email-safe HTML render helpers for the invitation
 * blocks that "Respond with meeting" and "Insert Talk link" drop
 * into an outgoing message body (#195).
 *
 * Constraints we design around:
 *   • Inline styles only.  Many mail clients strip <style>
 *     blocks, classes carry no meaning across clients.
 *   • System font stack — no @import, no remote @font-face.
 *   • Width capped at 560 px so the card doesn't blow out a
 *     mobile inbox view.
 *   • Detail-row glyphs are emoji (📅 🕐 📍 📝) because every
 *     client renders them; SVG / icon fonts inside email are
 *     unreliable across mail clients.
 *   • Banner image referenced via the project's GitHub raw URL
 *     so the recipient's mail client fetches a real PNG of the
 *     Unkai logo when they permit remote content.  Falls back
 *     gracefully to alt-text if blocked.
 *
 * The visual language across both cards is identical: same
 * gradient header, same surface card, same CTA button shape.
 * That keeps a thread carrying a Talk room *and* a meeting card
 * (the common case) reading as one consistent invitation,
 * not two unrelated stickers.
 */

/** Compatibility export — the brand logo lived here as a data
 *  URI for one cycle, before we discovered that many mail
 *  clients and corporate filters strip `<img src="data:…">`
 *  (and any remote URL hits remote-image blocking on first
 *  read). Both paths produced a broken-icon artifact for the
 *  recipient, so the chrome dropped the `<img>` entirely in
 *  favour of a typography-only wordmark.
 *
 *  Kept as an empty string so any external import resolves to
 *  something falsy without breaking the bundle; new code
 *  shouldn't reference it. Remove on next cleanup pass once we
 *  confirm nothing else imports it. */
export const PUBLIC_UNKAI_LOGO_URL = ''
/** Minimal HTML escape for the few values we splice into the
 *  template. Same characters DOMPurify-tier sanitisers cover —
 *  we don't need full sanitisation here because the inputs are
 *  app-side strings (event summary, room name, attendee list)
 *  rather than untrusted remote content. */
function esc(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

/** Format an ISO / Date pair as a single human line:
 *  "Tuesday, May 6 · 14:00 – 15:00".  Locale-aware via Intl,
 *  weekday-first because that's how every mature calendar
 *  invitation reads. */
function formatRange(start: Date, end: Date): string {
  const sameDay =
    start.getFullYear() === end.getFullYear() &&
    start.getMonth() === end.getMonth() &&
    start.getDate() === end.getDate()
  const dateFmt = new Intl.DateTimeFormat(undefined, {
    weekday: 'long',
    month: 'long',
    day: 'numeric',
  })
  const timeFmt = new Intl.DateTimeFormat(undefined, {
    hour: '2-digit',
    minute: '2-digit',
  })
  const datePart = dateFmt.format(start)
  if (sameDay) {
    return `${datePart} · ${timeFmt.format(start)} – ${timeFmt.format(end)}`
  }
  return `${datePart} ${timeFmt.format(start)} – ${dateFmt.format(end)} ${timeFmt.format(end)}`
}

// ── Shared design tokens (inlined into every style attribute) ──
//
// Tokens kept as string constants so a future tweak to the
// invite palette stays a single-file edit.  Values picked to
// read cleanly on both light and dark mail-client backgrounds —
// the card sits on its own white surface regardless.

const T = {
  card: 'background:#ffffff;border:1px solid #e2e8f0;border-radius:14px;box-shadow:0 1px 2px rgba(15,23,42,0.04),0 8px 24px rgba(15,23,42,0.06);overflow:hidden;',
  headerBg:
    'background:linear-gradient(135deg,#3b82f6 0%,#6366f1 100%);padding:20px 24px;color:#ffffff;',
  // Typography-only header: a wordmark inside a soft pill.
  // Many mail clients strip `<img src="data:…">` URIs for
  // security, and the previous remote-URL approach hit image-
  // blocking by default — both left users staring at a broken-
  // image icon. The pill + wordmark renders identically in every
  // client because it's pure inline-styled HTML.
  headerPill:
    'display:inline-block;padding:7px 14px;border-radius:999px;background:rgba(255,255,255,0.18);',
  headerWordmark:
    'font-size:13px;font-weight:700;letter-spacing:0.12em;color:#ffffff;text-transform:uppercase;',
  body: 'padding:24px;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;color:#0f172a;',
  title:
    'margin:0 0 4px 0;font-size:20px;line-height:1.3;font-weight:600;color:#0f172a;',
  subtitle:
    'margin:0;font-size:14px;line-height:1.5;color:#475569;',
  detailLabel:
    'font-size:11px;font-weight:600;letter-spacing:0.06em;text-transform:uppercase;color:#64748b;',
  detailValue:
    'font-size:14px;line-height:1.5;color:#0f172a;margin:2px 0 0 0;',
  divider: 'height:1px;background:#e2e8f0;margin:20px 0;border:0;',
  ctaRow: 'margin-top:24px;',
  ctaButton:
    'display:inline-block;background:#3b82f6;color:#ffffff;text-decoration:none;font-weight:600;font-size:14px;padding:11px 22px;border-radius:10px;letter-spacing:0.01em;',
  footer:
    'margin:20px 0 0 0;padding:14px 16px;background:#f8fafc;border-radius:10px;font-size:12px;line-height:1.5;color:#475569;',
  footerStrong: 'color:#0f172a;font-weight:600;',
} as const

/** Wrap the card body inside the shared chrome (outer wrapper +
 *  branded header). The header is a typography-only wordmark in
 *  a soft pill — no image, because many mail clients and
 *  corporate filters strip `<img src="data:…">` for security,
 *  and remote URLs hit image-blocking by default, so any image-
 *  based brand element ends up as a broken-icon artifact for the
 *  recipient. The wordmark renders identically in every client.
 *
 *  The outer `data-unkai-block` attribute is the marker the
 *  RichTextEditor's `UnkaiBlock` extension parses into an atom
 *  node — that's how the styled card survives Tiptap's schema
 *  unwrapping when the HTML is loaded into the editor. */
function chrome(bodyHtml: string, kind: string): string {
  return `
<div data-unkai-block="${kind}" style="max-width:560px;margin:16px 0;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;">
  <div style="${T.card}">
    <div style="${T.headerBg}">
      <span style="${T.headerPill}"><span style="${T.headerWordmark}">Unkai Mail</span></span>
    </div>
    <div style="${T.body}">
      ${bodyHtml}
    </div>
  </div>
</div>
`.trim()
}

/** One detail row: emoji + label + value.  Stacked vertically
 *  so the wrap-around case (long location, long Talk URL) reads
 *  cleanly instead of forcing horizontal scroll. */
function detailRow(emoji: string, label: string, value: string): string {
  return `
<div style="display:flex;gap:12px;align-items:flex-start;margin:14px 0;">
  <div style="font-size:18px;line-height:1.4;flex-shrink:0;width:24px;text-align:center;">${emoji}</div>
  <div style="flex:1;min-width:0;">
    <div style="${T.detailLabel}">${esc(label)}</div>
    <div style="${T.detailValue}">${value}</div>
  </div>
</div>
`.trim()
}

/** Footer microcopy block — the "You'll be invited via
 *  Nextcloud" line lives here so it reads as a system-level
 *  note rather than competing with the meeting title. */
function nextcloudFooter(message: string): string {
  return `
<div style="${T.footer}">
  <span style="${T.footerStrong}">📨 ${esc(message)}</span><br />
  <span>Accept the invitation in your mail client or directly in Nextcloud — your calendar updates either way.</span>
</div>
`.trim()
}

// ── Public renderers ────────────────────────────────────────────

export interface TalkInvite {
  /** Display name of the Talk room. */
  name: string
  /** Full join URL the recipient will click. */
  url: string
}

/** Render-time options for the invite cards.  Currently empty —
 *  the chrome no longer carries a logo image, so there's nothing
 *  to override.  Kept as an exported type so existing call sites
 *  that pass `{}` keep type-checking and we have a place to add
 *  knobs later (theme override, etc.) without churning every
 *  signature. */
export type InviteRenderOptions = Record<string, never>

/** Render a Talk-meeting invitation card.  Used by Compose's
 *  "Insert Talk link" flow when the user attaches a freshly-
 *  created Talk room to an outgoing message. */
export function talkInviteHtml(invite: TalkInvite, _opts: InviteRenderOptions = {}): string {
  const inner = `
<h1 style="${T.title}">You're invited to a Talk meeting</h1>
<p style="${T.subtitle}">Click the button below to join the conversation in Nextcloud Talk — works in any modern browser, no install.</p>

<hr style="${T.divider}" />

${detailRow(
  '💬',
  'Talk room',
  `<strong style="font-weight:600;">${esc(invite.name)}</strong>`,
)}
${detailRow(
  '🔗',
  'Join link',
  `<a href="${esc(invite.url)}" style="color:#3b82f6;text-decoration:none;word-break:break-all;">${esc(invite.url)}</a>`,
)}

<div style="${T.ctaRow}">
  <a href="${esc(invite.url)}" style="${T.ctaButton}">Join Talk meeting →</a>
</div>

${nextcloudFooter("You'll also be added as a participant in Nextcloud Talk.")}
`.trim()
  return chrome(inner, 'talk-invite')
}

export interface MeetingInvite {
  /** Event title / summary line. */
  summary: string
  /** Event start (Date or ISO string the constructor accepts). */
  start: Date | string
  /** Event end (Date or ISO string). */
  end: Date | string
  /** Optional location text (free-form — a room name, address, or
   *  Talk URL the user typed into the Location field). */
  location?: string | null
  /** Optional notes / description.  Plain text; we wrap-preserve
   *  newlines so a multi-line agenda survives the render. */
  description?: string | null
  /** Optional Talk room URL.  Rendered as its own row + a second
   *  CTA button so the recipient can join the conversation
   *  ahead of the meeting if they want. */
  talkUrl?: string | null
}

/** Render a calendar-meeting invitation card.  Used after the
 *  user saves an event from the "Respond with meeting" flow —
 *  Compose opens with this block pre-filled in the body. */
export function meetingInviteHtml(invite: MeetingInvite, _opts: InviteRenderOptions = {}): string {
  const start = invite.start instanceof Date ? invite.start : new Date(invite.start)
  const end = invite.end instanceof Date ? invite.end : new Date(invite.end)

  const detailRows: string[] = [
    detailRow(
      '📅',
      'When',
      `<strong style="font-weight:600;">${esc(formatRange(start, end))}</strong>`,
    ),
  ]
  if (invite.location && invite.location.trim()) {
    detailRows.push(detailRow('📍', 'Where', esc(invite.location.trim())))
  }
  if (invite.talkUrl && invite.talkUrl.trim()) {
    detailRows.push(
      detailRow(
        '💬',
        'Talk room',
        `<a href="${esc(invite.talkUrl)}" style="color:#3b82f6;text-decoration:none;word-break:break-all;">${esc(invite.talkUrl)}</a>`,
      ),
    )
  }
  if (invite.description && invite.description.trim()) {
    // Preserve plain-text newlines as <br> so an agenda with
    // line breaks reads correctly.  The text content goes
    // through `esc` first so any `<` `>` `&` in the user's
    // notes don't accidentally break the surrounding HTML.
    const notesHtml = esc(invite.description.trim()).replace(/\r?\n/g, '<br />')
    detailRows.push(detailRow('📝', 'Notes', notesHtml))
  }

  const ctaHref = invite.talkUrl && invite.talkUrl.trim() ? invite.talkUrl : null
  const ctaBlock = ctaHref
    ? `
<div style="${T.ctaRow}">
  <a href="${esc(ctaHref)}" style="${T.ctaButton}">Join Talk meeting →</a>
</div>
`.trim()
    : ''

  const inner = `
<h1 style="${T.title}">${esc(invite.summary)}</h1>
<p style="${T.subtitle}">A calendar invitation has been created in Nextcloud and shared with everyone on this thread.</p>

<hr style="${T.divider}" />

${detailRows.join('\n')}

${ctaBlock}

${nextcloudFooter("You'll be invited via Nextcloud — accepting in your mail client adds the event to your calendar.")}
`.trim()
  return chrome(inner, 'meeting-invite')
}

// ── Quoted-history wrapper (#195 follow-up) ─────────────────────

/** Wrap a reply's quoted history in a styled container.  Visually
 *  pushes the previous thread back so the user's fresh reply
 *  reads first: muted background, smaller type, soft left border,
 *  generous padding.  Same subdued grey palette across light and
 *  dark themes — designed to read as "old context" rather than
 *  competing with the new content.
 *
 *  The outer `data-unkai-block` attribute lets the editor's
 *  `UnkaiBlock` extension capture this as an atom node so the
 *  styled wrapper survives Tiptap's schema (which would otherwise
 *  unwrap the `<div>` and strip the inline styles).
 *
 *  The inner content is also wrapped in a `<blockquote
 *  type="cite">` (#277).  Standard mail clients use blockquote
 *  as the canonical "this is quoted from a previous mail"
 *  signal — Apple Mail, Thunderbird, Gmail, Outlook each render
 *  blockquoted content with their own indent / collapse
 *  affordance ("Show original", a vertical bar, etc.).  The
 *  `type="cite"` attribute is the long-standing convention for
 *  email-quoted blockquotes (vs. literary citations); it does
 *  nothing visual but a couple of clients use it as a stronger
 *  signal that the block is a mail quote.  We zero out the
 *  blockquote's default browser indent so it doesn't double
 *  up with the styled div's own padding. */
export function quotedHistoryHtml(args: {
  fromHeader: string
  whenText: string
  bodyHtml: string
}): string {
  const { fromHeader, whenText, bodyHtml } = args
  const escAttr = (s: string) =>
    s
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;')
  const meta = `On ${escAttr(whenText)}, ${escAttr(fromHeader)} wrote:`
  return `
<div data-unkai-block="quoted-history" style="margin:24px 0 0 0;padding:14px 18px;background:#f1f5f9;border-left:3px solid #cbd5e1;border-radius:8px;color:#64748b;font-size:13px;line-height:1.55;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;">
  <div style="font-size:11px;font-weight:600;letter-spacing:0.06em;text-transform:uppercase;color:#94a3b8;margin-bottom:8px;">Previous conversation</div>
  <div style="font-size:12px;color:#475569;margin-bottom:10px;">${meta}</div>
  <blockquote type="cite" style="margin:0;padding:0;border:0;color:#64748b;">${bodyHtml}</blockquote>
</div>
`.trim()
}

// ── Forwarded-mail wrapper (#319) ───────────────────────────────

/** Wrap a forwarded message's body so the original formatting
 *  is preserved verbatim and the header matches the long-standing
 *  forwarded-mail convention every receiving mail client knows.
 *
 *  The header is the de-facto-standard plain-text delimiter
 *  (`---------- Forwarded message ----------`) followed by
 *  bold-labelled `From:` / `Date:` / `Subject:` / `To:` lines.
 *  This is the same shape virtually every other mail client emits,
 *  so the recipient's client renders the forward the way they
 *  expect rather than seeing a bespoke chrome from us.
 *
 *  The `data-unkai-block` wrapper carries no visual styling — it
 *  exists purely so the editor's `UnkaiBlock` extension captures
 *  the whole chunk as an atom node. Without that, Tiptap's
 *  StarterKit schema unwraps the wrapper, strips the original
 *  message's inline styles, and can duplicate `<img>` nodes while
 *  reconciling complex `<table>` / `<picture>` markup against its
 *  block schema (the original visual symptom on #319). Inside the
 *  atom Tiptap doesn't parse the interior, so the email's table
 *  layout, inline styles, and `<img>` elements survive untouched.
 *  The chunk is non-editable; users add their note above it and
 *  remove it as a unit with one Backspace if they don't want it.
 *
 *  `toHeader` is optional because the field is occasionally
 *  missing on cached payloads — when absent we just omit the line
 *  rather than print an empty `To:`. */
export function forwardedMailHtml(args: {
  fromHeader: string
  whenText: string
  subjectText: string
  toHeader?: string
  bodyHtml: string
}): string {
  const { fromHeader, whenText, subjectText, toHeader, bodyHtml } = args
  const esc = (s: string) =>
    s
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
  const lines = [
    `<div><b>From:</b> ${esc(fromHeader)}</div>`,
    `<div><b>Date:</b> ${esc(whenText)}</div>`,
    `<div><b>Subject:</b> ${esc(subjectText)}</div>`,
  ]
  if (toHeader && toHeader.trim()) {
    lines.push(`<div><b>To:</b> ${esc(toHeader)}</div>`)
  }
  return `
<div data-unkai-block="forwarded-mail">
  <p>---------- Forwarded message ----------</p>
  ${lines.join('\n  ')}
  <p></p>
  ${bodyHtml}
</div>
`.trim()
}

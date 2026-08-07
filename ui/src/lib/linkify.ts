/**
 * linkify — split a plain-text string into text / link segments so
 * the UI can render `http(s)://…` URLs in user-entered text (event
 * descriptions, agendas) as clickable anchors.
 *
 * The caller renders each segment through the template as *text*
 * (never `innerHTML`), so linkifying introduces no injection
 * surface: a description containing `<script>` stays inert text,
 * only recognised URLs become `<a>` elements with the URL itself
 * as the href.
 *
 * Matching mirrors `extract_meeting_url` / CalendarView's inline
 * matcher: any `http(s)://` token counts — no conferencing-platform
 * whitelist, that bitrots quickly. Trailing punctuation that's
 * almost certainly prose rather than URL (`(see https://x.example).`)
 * is trimmed off the match, with one refinement: a closing `)` is
 * kept while the URL body has an unmatched `(`, so wiki-style URLs
 * like `https://x.example/Foo_(bar)` survive intact.
 */

export interface TextSegment {
  kind: 'text'
  text: string
}

export interface LinkSegment {
  kind: 'link'
  /** The exact substring of the input to display. */
  text: string
  /** Same as `text` — only absolute http(s) URLs are matched. */
  href: string
}

export type LinkifySegment = TextSegment | LinkSegment

const URL_RE = /https?:\/\/[^\s<>"']+/gi

/** Punctuation that ends a sentence around a URL more often than it
 *  ends the URL itself. `)` is handled separately (paren balance). */
const TRAILING_PUNCTUATION = '.,;:!?]'

function trimTrailingPunctuation(url: string): string {
  for (;;) {
    const last = url[url.length - 1]
    if (last === undefined) break
    if (TRAILING_PUNCTUATION.includes(last)) {
      url = url.slice(0, -1)
      continue
    }
    if (last === ')') {
      const opens = url.split('(').length - 1
      const closes = url.split(')').length - 1
      if (closes > opens) {
        url = url.slice(0, -1)
        continue
      }
    }
    break
  }
  return url
}

export function linkify(text: string): LinkifySegment[] {
  const segments: LinkifySegment[] = []
  let cursor = 0
  URL_RE.lastIndex = 0
  for (const match of text.matchAll(URL_RE)) {
    const url = trimTrailingPunctuation(match[0])
    // A bare "http://" with everything trimmed away isn't a link.
    if (!/^https?:\/\/.+/i.test(url)) continue
    if (match.index > cursor) {
      segments.push({ kind: 'text', text: text.slice(cursor, match.index) })
    }
    segments.push({ kind: 'link', text: url, href: url })
    cursor = match.index + url.length
  }
  if (cursor < text.length) {
    segments.push({ kind: 'text', text: text.slice(cursor) })
  }
  return segments
}

/** True when `text` contains at least one linkifiable URL. */
export function hasLinks(text: string): boolean {
  return linkify(text).some((s) => s.kind === 'link')
}

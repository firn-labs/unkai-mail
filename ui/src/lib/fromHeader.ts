/**
 * Parse an RFC 5322 "From"-style header into a display name and email
 * address.  The cached EmailEnvelope only carries the From header as a
 * single raw string (see `unkai-core::models::EmailEnvelope.from`),
 * so any consumer that wants to look the sender up in the contact
 * cache has to extract the email itself.
 *
 * Inputs we handle:
 *   `Alice Smith <alice@example.com>`     → name "Alice Smith", email "alice@example.com"
 *   `"Smith, Alice" <alice@example.com>`  → name "Smith, Alice", email "alice@example.com"
 *   `alice@example.com`                   → name "", email "alice@example.com"
 *   `(garbage with no @)`                 → name "(garbage with no @)", email ""
 *   `""` / `null` / `undefined`           → name "", email ""
 *
 * RFC 2047 encoded-word names (`=?UTF-8?Q?…?=`) are intentionally NOT
 * decoded here — the IMAP layer is expected to hand the UI a decoded
 * header.  Anything that still arrives encoded just falls through as
 * the raw text and the avatar's initial-letter fallback degrades
 * gracefully.
 */
export interface ParsedFrom {
  name: string
  email: string
}

const ANGLE_RE = /^(.*?)\s*<([^>]+)>\s*$/
const QUOTED_RE = /^"(.*)"$/

export function parseFromHeader(raw: string | null | undefined): ParsedFrom {
  const s = (raw ?? '').trim()
  if (!s) return { name: '', email: '' }

  const m = ANGLE_RE.exec(s)
  if (m) {
    let name = m[1].trim()
    const q = QUOTED_RE.exec(name)
    if (q) name = q[1]
    return { name, email: m[2].trim() }
  }

  // No angle brackets: a bare email like `alice@example.com`, or a
  // display-name-only header (rare in practice but possible for
  // automated test data).
  if (s.includes('@')) return { name: '', email: s }
  return { name: s, email: '' }
}

/** Pick the best label for an avatar / sender chip — prefer a parsed
 *  display name, otherwise the local-part of the email so a stray
 *  `alice@example.com` still becomes a recognisable "A". */
export function senderLabel(p: ParsedFrom): string {
  if (p.name) return p.name
  if (p.email) {
    const at = p.email.indexOf('@')
    return at > 0 ? p.email.slice(0, at) : p.email
  }
  return ''
}

// RFC 6068 `mailto:` URL parsing for routing inbound mail links
// (in-app anchor clicks + OS-level handler launches) into the
// in-app Compose flow.  Pulled out of MailView so the notes
// preview pane (#294) and the Rust-side OS deep-link bridge can
// share one tolerant parser.
//
// Tolerant of the wild — missing pieces stay `undefined` so the
// caller's defaults take over.  Multiple `to=` / `cc=` / `bcc=`
// query params accumulate (the spec allows it); `subject` /
// `body` are last-write-wins because every real-world generator
// only emits one of each.
//
// Decoding goes through `decodeURIComponent` with a `+` → `%20`
// pre-pass (some senders use the form-encoding convention even
// though RFC 6068 says spaces should be `%20`); a decode failure
// falls back to the raw token so a malformed mailto still opens
// compose with whatever survived.

export interface MailtoParsed {
  to?: string
  cc?: string
  bcc?: string
  subject?: string
  body?: string
}

export function parseMailtoUrl(raw: string): MailtoParsed {
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
  const out: MailtoParsed = {}
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

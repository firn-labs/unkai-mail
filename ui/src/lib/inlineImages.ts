/**
 * Inline body images (#471) — resolving `<img src="cid:…">` against
 * the message's own MIME parts.
 *
 * A sender that wants an image *in* the body (a signature logo, a
 * newsletter header) attaches it with a Content-ID and points at it
 * with an RFC 2392 `cid:` URL. The webview has no idea what a `cid:`
 * URL is, so without this pass those images render as nothing — the
 * bug #471 describes.
 *
 * The backend hands us the bytes of every referenceable image part
 * (`fetch_inline_images`); this module turns them into object URLs
 * and rewrites the body's `cid:` sources to point at those.
 *
 * Kept out of the components so the matching rules are unit-testable
 * and so both readers — the reading pane and the `.eml` popout —
 * resolve cids identically.
 */

// The part shape itself is a backend DTO, so it lives with the other
// command payload types in `api/types.ts`; re-exported here so
// consumers of these helpers get both from one import.
import type { InlineImagePart } from './api/types'
export type { InlineImagePart }

/**
 * Normalise anything that identifies an inline part — a `cid:` URL, a
 * bare Content-ID, a filename — into a lookup key.
 *
 * Three normalisations, each earning its place against real mail:
 *  - drop a leading `cid:` scheme, so callers can pass a raw `src`;
 *  - strip `<…>`, because senders copy the Content-ID *header* value
 *    (which carries the brackets) into the body's URL;
 *  - percent-decode and lowercase, since RFC 2392 URLs are encoded
 *    (`cid:logo%40example`) and Content-IDs compare case-insensitively.
 */
export function inlineImageKey(raw: string): string {
  let s = raw.trim()
  if (s.toLowerCase().startsWith('cid:')) s = s.slice(4).trim()
  s = s.replace(/^<|>$/g, '').trim()
  try {
    s = decodeURIComponent(s)
  } catch {
    // Malformed percent-escapes ("50% off.png") — the undecoded
    // form is still a usable key, and matching on it is strictly
    // better than dropping the image.
  }
  return s.toLowerCase()
}

/**
 * Build the `key → URL` map the rewrite pass looks parts up in.
 *
 * `toUrl` is injected rather than calling `URL.createObjectURL`
 * directly so the caller owns the lifetime of what it creates (it has
 * to revoke those URLs when the message closes) and so this stays
 * testable without a DOM.
 *
 * Each part is registered under its Content-ID *and* its filename:
 * some senders write `<img src="cid:logo.png">` naming the file
 * rather than the Content-ID. Content-IDs win — a filename key never
 * overwrites one, and never overwrites an earlier part's filename,
 * so with two `logo.png` attachments the first one referenced wins
 * instead of the last one parsed.
 */
export function buildInlineImageUrls(
  parts: InlineImagePart[],
  toUrl: (part: InlineImagePart) => string,
): Record<string, string> {
  const byCid: Record<string, string> = {}
  const byFilename: Record<string, string> = {}
  for (const part of parts) {
    const url = toUrl(part)
    if (part.contentId) {
      const key = inlineImageKey(part.contentId)
      if (key && !(key in byCid)) byCid[key] = url
    }
    if (part.filename) {
      const key = inlineImageKey(part.filename)
      if (key && !(key in byFilename)) byFilename[key] = url
    }
  }
  return { ...byFilename, ...byCid }
}

/**
 * Rewrite every `<img src="cid:…">` in `doc` to the resolved URL.
 *
 * `loading` says whether the backend fetch is still in flight, and
 * decides what an *unresolved* image looks like:
 *  - while loading, we blank the `src` attribute out to a marker
 *    attribute so the webview doesn't paint a broken-image icon for
 *    the second or two the fetch takes;
 *  - once loading is done, an image we still can't resolve keeps its
 *    `alt` text and loses its `src` entirely — the part genuinely
 *    isn't in the message (or was over the size ceiling), and alt
 *    text is more use than an invisible gap.
 *
 * Returns the number of images it resolved, which the caller uses to
 * decide whether the rewrite was worth anything.
 */
export function applyInlineImages(
  doc: Document,
  urls: Record<string, string>,
  loading: boolean,
): number {
  let resolved = 0
  doc.querySelectorAll('img[src]').forEach((img) => {
    const src = img.getAttribute('src') ?? ''
    if (!src.toLowerCase().startsWith('cid:')) return
    const key = inlineImageKey(src)
    const url = urls[key]
    // Record which part the element wanted, resolved or not — the
    // `src` we're about to write is an opaque `blob:` URL, so this
    // is the only thing left in the rendered DOM that says why an
    // image is (or isn't) there.
    img.setAttribute('data-unkai-cid', key)
    if (url) {
      img.setAttribute('src', url)
      resolved += 1
      return
    }
    img.removeAttribute('src')
    // `srcset` would resurrect the unresolvable cid through the
    // responsive-image path.
    img.removeAttribute('srcset')
    if (loading) img.setAttribute('data-unkai-inline-pending', '1')
  })
  return resolved
}

/**
 * Every distinct cid an `<img>` in `doc` references. The reader uses
 * this purely as a gate — no cid images means no backend round-trip
 * at all, which is the overwhelmingly common case for plain mail.
 */
export function collectCidImageRefs(doc: Document): string[] {
  const seen = new Set<string>()
  doc.querySelectorAll('img[src]').forEach((img) => {
    const src = img.getAttribute('src') ?? ''
    if (!src.toLowerCase().startsWith('cid:')) return
    const key = inlineImageKey(src)
    if (key) seen.add(key)
  })
  return [...seen]
}

/**
 * Decode one part's base64 payload into a Blob the caller can turn
 * into an object URL.
 *
 * `atob` yields a binary string (one char per byte); the `Uint8Array`
 * hop is what turns it back into real bytes. Anything malformed
 * throws, and callers treat that as "this image just doesn't render".
 */
export function inlineImageBlob(part: InlineImagePart): Blob {
  const binary = atob(part.base64)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i)
  return new Blob([bytes], { type: part.mime || 'application/octet-stream' })
}

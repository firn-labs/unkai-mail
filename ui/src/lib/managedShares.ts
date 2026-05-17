/**
 * Helpers for the "Nextcloud share links registered to a Compose
 * draft" cleanup flow (#193).
 *
 * Compose stamps each share anchor it inserts into the email body
 * with `data-unkai-share-id` and `data-unkai-share-nc`.  When the
 * draft is later cancelled or deleted, the cleanup pass parses
 * those markers back out of the body and calls
 * `delete_nextcloud_share` for each one so the user's "Shared with
 * others" list doesn't accumulate orphans.
 *
 * The markers live on `<a>` elements only — we never scan plain
 * text URLs the user may have pasted, because we have no idea who
 * minted those or whether the user wants them deleted.
 */

export interface ManagedShareRef {
  /** Nextcloud account id the share belongs to. */
  ncId: string
  /** Numeric share id used by the OCS API. */
  shareId: string
}

/**
 * Pull every `<a data-unkai-share-id="…" data-unkai-share-nc="…">`
 * marker out of an HTML string.  Robust to attribute order (the two
 * `data-` attributes can come in either order on the same element)
 * and to whitespace inside the tag.
 *
 * Returns deduplicated entries — if the same share id appears twice
 * (a user copy-pasted the snippet), we delete it once.
 */
export function extractManagedShares(html: string | null | undefined): ManagedShareRef[] {
  if (!html) return []
  // Match an <a …> opening tag that carries both attributes, in
  // either order.  Captures the share id and the nc id.  We use
  // two passes (one per attribute order) and merge — a single
  // unified regex would be hard to read.
  const out: ManagedShareRef[] = []
  const seen = new Set<string>()
  const tagRe = /<a\b[^>]*>/gi
  for (const m of html.matchAll(tagRe)) {
    const tag = m[0]
    const idMatch = tag.match(/\bdata-unkai-share-id="([^"]*)"/i)
    const ncMatch = tag.match(/\bdata-unkai-share-nc="([^"]*)"/i)
    if (!idMatch || !ncMatch) continue
    const shareId = idMatch[1]
    const ncId = ncMatch[1]
    if (!shareId || !ncId) continue
    const key = `${ncId}::${shareId}`
    if (seen.has(key)) continue
    seen.add(key)
    out.push({ ncId, shareId })
  }
  return out
}

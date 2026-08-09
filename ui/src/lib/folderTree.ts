/**
 * Folder-hierarchy presentation logic for the mail sidebar (#478).
 *
 * IMAP LIST hands us a flat array of full paths ("Projects/2026")
 * plus each folder's hierarchy delimiter. The sidebar wants to show
 * subfolders directly underneath their parent, indented one step per
 * level, while keeping its two-tier layout: special-use folders in
 * canonical order on top, user folders alphabetically below.
 *
 * This module is deliberately pure (no Svelte, no DOM) so the
 * nesting rules stay unit-tested — same split as `inlineImages.ts`
 * and `emailContrast.ts`.
 */

/** The subset of the sidebar's `Folder` shape the tree logic needs. */
export interface TreeFolderLike {
  name: string
  delimiter: string | null
}

/** One row of the flattened render list: the folder plus how many
 *  indent steps it sits from its tier's left edge. */
export interface FolderRow<F extends TreeFolderLike> {
  folder: F
  depth: number
}

/**
 * Flatten a server folder list into the two render tiers.
 *
 * - `standardRoots` are the special-use folders (plus any synthetic
 *   entries like the local Outbox), already sorted in canonical
 *   order by the caller. Each renders at depth 0 in the standard
 *   tier, immediately followed by its user-folder descendants.
 * - Every folder in `folders` that is not standard nests under its
 *   *nearest existing ancestor* — so "Projects/2026" indents under
 *   "Projects", and "INBOX/Work" indents under the Inbox even
 *   though Inbox lives in the standard tier. A subfolder whose
 *   parent the server never listed (some servers omit \NonExistent
 *   intermediates) falls back to the next ancestor up, or becomes a
 *   top-level row in the custom tier.
 * - Special-use wins over path: on servers that prefix everything
 *   with the inbox namespace ("INBOX.Sent"), the Sent folder stays
 *   a depth-0 standard root instead of indenting under Inbox.
 *
 * Siblings at every level are ordered with `compare` (the caller
 *   passes its locale-aware leaf-name comparison).
 */
export function buildFolderRows<F extends TreeFolderLike>(
  folders: F[],
  standardRoots: F[],
  isStandard: (f: F) => boolean,
  compare: (a: F, b: F) => number,
): { standard: FolderRow<F>[]; custom: FolderRow<F>[] } {
  // Parent lookup considers every known name — real folders and
  // synthetic standard roots alike — so children can attach to
  // either tier.
  const known = new Set<string>()
  for (const f of folders) known.add(f.name)
  for (const f of standardRoots) known.add(f.name)

  /** parent full path → its direct (display-)children */
  const children = new Map<string, F[]>()
  const customRoots: F[] = []

  for (const f of folders) {
    if (isStandard(f)) continue
    const delim = f.delimiter ?? '/'
    // Walk ancestors from the longest prefix down until one exists.
    let parent: string | null = null
    let cut = f.name.lastIndexOf(delim)
    while (cut > 0) {
      const candidate = f.name.slice(0, cut)
      if (known.has(candidate)) {
        parent = candidate
        break
      }
      cut = candidate.lastIndexOf(delim)
    }
    if (parent) {
      const siblings = children.get(parent)
      if (siblings) siblings.push(f)
      else children.set(parent, [f])
    } else {
      customRoots.push(f)
    }
  }

  function emit(f: F, depth: number, out: FolderRow<F>[]) {
    out.push({ folder: f, depth })
    const kids = children.get(f.name)
    if (kids) {
      for (const child of [...kids].sort(compare)) emit(child, depth + 1, out)
    }
  }

  const standard: FolderRow<F>[] = []
  for (const root of standardRoots) emit(root, 0, standard)

  const custom: FolderRow<F>[] = []
  for (const root of [...customRoots].sort(compare)) emit(root, 0, custom)

  return { standard, custom }
}

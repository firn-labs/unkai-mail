/**
 * CodeMirror 6 trigger-detection plugin for the Notes editor (#260).
 *
 * Watches the document + selection on every transaction and, if the
 * cursor sits at the end of a `@<query>` or `/mail <query>` token,
 * emits a `MentionContext` to the parent (the Svelte editor wrapper).
 * The parent owns the actual popup UI — keeping the React-vs-Svelte
 * boundary out of CM6 internals means we can render the picker with
 * the same component idioms as the rest of the app.
 *
 * # Why not `@codemirror/autocomplete`?
 *
 * Its built-in popup is a generic vertical list — labels only, no
 * room for avatars / subject + preview / per-row metadata.  Issue #260
 * asks the picker to match the Compose `@` picker, which has rich
 * rows.  Writing our own pin-and-emit plugin and rendering the popup
 * in Svelte is straightforward; bending CM6's popup to look like the
 * rest of the app is not.  We still use CM6's `coordsAtPos` to anchor
 * the popup, so the positioning Just Works through CM6's scroll /
 * folding logic.
 *
 * # Trigger grammar
 *
 * - `@<word>`        — `@` immediately preceded by start-of-line OR
 *                      whitespace.  Avoids firing inside email
 *                      addresses (`foo@bar.com`) or @-handles inside
 *                      code blocks.  Stops at the next whitespace.
 *
 * - `/mail <words>`  — `/mail` immediately preceded by start-of-line
 *                      OR whitespace, followed by a single space and
 *                      an optional query.  Query can contain spaces
 *                      so the user can type `/mail project budget`;
 *                      the trigger ends at the line boundary.
 *
 * The regexes are intentionally permissive: the popup just stays
 * closed when there are no items, so a stray `@` in prose doesn't
 * look broken.
 */

import { ViewPlugin, type EditorView, type ViewUpdate } from '@codemirror/view'
import type { Extension } from '@codemirror/state'

export type MentionType = 'contact' | 'mail'

/** What the trigger plugin emits whenever the cursor sits in a
 *  mention context.  `from`/`to` are document offsets covering the
 *  trigger plus its query, so the consumer can replace the whole
 *  span when the user picks an item.  `coords` is the on-screen
 *  position of the trigger character's left edge — the popup uses
 *  it as the anchor for its top-left corner. */
export interface MentionContext {
  type: MentionType
  query: string
  from: number
  to: number
  coords: { left: number; top: number; bottom: number }
}

interface PluginOpts {
  /** Fired whenever the mention context changes — either a new
   *  trigger appeared, the query updated, or the trigger went away
   *  (in which case `ctx` is `null`).  Cheap; called on every
   *  selection-bearing update. */
  onContextChange: (ctx: MentionContext | null) => void
}

/** `@foo` — `@` after start-of-line or whitespace, followed by a
 *  query of non-whitespace word chars (letters, digits, `_`, `-`,
 *  `.`, `'`).  Match is anchored with a lookbehind via the
 *  `before` check in `detect()` since JS regex lookbehind isn't
 *  fully portable across older WebViews we support. */
const CONTACT_TRIGGER = /(@)([A-Za-z0-9._'-]*)$/

/** `/mail <query>` — same start-of-token guard, plus the literal
 *  `mail` + a single space.  Query can contain spaces so we match
 *  to end-of-line. */
const MAIL_TRIGGER = /(\/mail) (.*)$/

/** True iff `prev` is a "boundary" character — start of doc,
 *  whitespace, or one of the markdown punctuation marks that
 *  reliably separate inline tokens.  Keeps `@foo` and `/mail` from
 *  firing inside `foo@bar.com` or a URL path like `/home/foo`. */
function isBoundary(prev: string | null): boolean {
  if (prev === null) return true
  return /\s|[>([\]]/.test(prev)
}

/** Run both trigger regexes against the line ending at the cursor;
 *  return the first match that survives the boundary check.  We
 *  prefer the `/mail` match over `@` because `/mail` is a longer
 *  literal and there's no overlap, but the boundary check makes
 *  this academic. */
function detect(view: EditorView): MentionContext | null {
  const { state } = view
  const sel = state.selection.main
  // Only fire on a collapsed cursor — multi-char selections aren't
  // typing context, they're editing context.
  if (!sel.empty) return null
  const head = sel.head
  const line = state.doc.lineAt(head)
  const lineText = line.text
  const offsetInLine = head - line.from
  const upToCursor = lineText.slice(0, offsetInLine)

  // Try /mail first — its anchor is `/mail<space>` so it can't
  // ever be confused with an `@` mention.
  const mailMatch = upToCursor.match(MAIL_TRIGGER)
  if (mailMatch) {
    const start = upToCursor.length - mailMatch[0].length
    const prev = start > 0 ? upToCursor[start - 1] : null
    if (isBoundary(prev)) {
      const from = line.from + start
      const to = head
      const coords = view.coordsAtPos(from)
      if (!coords) return null
      return {
        type: 'mail',
        query: mailMatch[2],
        from,
        to,
        coords: { left: coords.left, top: coords.top, bottom: coords.bottom },
      }
    }
  }

  const contactMatch = upToCursor.match(CONTACT_TRIGGER)
  if (contactMatch) {
    const start = upToCursor.length - contactMatch[0].length
    const prev = start > 0 ? upToCursor[start - 1] : null
    if (isBoundary(prev)) {
      const from = line.from + start
      const to = head
      const coords = view.coordsAtPos(from)
      if (!coords) return null
      return {
        type: 'contact',
        query: contactMatch[2],
        from,
        to,
        coords: { left: coords.left, top: coords.top, bottom: coords.bottom },
      }
    }
  }
  return null
}

/** Build the CM6 extension.  One ViewPlugin per editor instance
 *  (each Notes editor gets its own).  The plugin keeps a small
 *  "last emitted" cache so we don't fire the callback on
 *  cursor-only moves that don't change the trigger context. */
export function createMentionExtension(opts: PluginOpts): Extension {
  return ViewPlugin.define((view) => {
    let last: MentionContext | null = null
    function maybeEmit(): void {
      const next = detect(view)
      if (sameContext(last, next)) return
      last = next
      opts.onContextChange(next)
    }
    // Fire once on construction so the popup is in sync with the
    // initial state — usually a no-op (cursor at start of empty
    // doc), but cheap.
    maybeEmit()
    return {
      update(u: ViewUpdate) {
        if (u.docChanged || u.selectionSet || u.geometryChanged) {
          maybeEmit()
        }
      },
    }
  })
}

function sameContext(
  a: MentionContext | null,
  b: MentionContext | null,
): boolean {
  if (a === null || b === null) return a === b
  return (
    a.type === b.type &&
    a.query === b.query &&
    a.from === b.from &&
    a.to === b.to &&
    a.coords.left === b.coords.left &&
    a.coords.top === b.coords.top
  )
}

/** Replace the trigger span with the chosen markdown link and
 *  position the cursor right after the insertion.  Centralised
 *  here (rather than in the consumer) so any future change to the
 *  insertion semantics — trailing space, smart capitalisation,
 *  etc. — lives next to the trigger grammar that justifies it. */
export function insertMention(
  view: EditorView,
  ctx: MentionContext,
  markdown: string,
): void {
  // Append a trailing space so the user can keep typing without
  // re-triggering the picker on the very next keystroke (the
  // boundary check sees the space and stays closed).
  const insert = `${markdown} `
  view.dispatch({
    changes: { from: ctx.from, to: ctx.to, insert },
    selection: { anchor: ctx.from + insert.length },
  })
  view.focus()
}

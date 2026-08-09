/**
 * Coordinate-space conversions for a zoomed UI (#191, #480).
 *
 * The webview exposes two pixel coordinate spaces, and they stop
 * being the same the moment the UI-scale zoom (#191, CSS `zoom` on
 * the document root) is anything other than 1.0:
 *
 *  - **visual pixels** — what `MouseEvent.clientX/Y`,
 *    `getBoundingClientRect()` and `window.innerWidth` report.
 *    These describe where things *are on screen*.
 *  - **layout pixels** — what CSS `left/top/width/height`,
 *    `offsetWidth/Height` and our px-per-minute grid constants
 *    mean.  These describe where things *are placed in the
 *    document*, including `position: fixed` overlays, which live
 *    in the zoomed root's layout space.
 *
 * Feeding a visual coordinate into a layout slot (anchoring a
 * context menu at `e.clientX`, mapping `clientY - rect.top` into a
 * time grid) lands off-target by exactly the zoom factor.  Every
 * helper here converts explicitly between the two spaces — and
 * every one reduces to the identity at 100 % scale, so callers pay
 * nothing when no zoom is active.
 *
 * The conversion factor is *measured*, not read from settings: we
 * compare an element's layout size (`offsetWidth/Height`) against
 * its visual size (`getBoundingClientRect()`).  That stays correct
 * on every webview engine we ship to (WebView2, WKWebView,
 * WebKitGTK) regardless of how each maps zoom into client
 * coordinates, and it transparently absorbs any other uniform
 * ancestor scaling that may exist besides the #191 zoom.
 *
 * House rules (see CLAUDE.md "Menu anchor pattern"):
 *  - anchoring a `position: fixed` overlay at the cursor →
 *    `cursorAnchor(e)`
 *  - anchoring a `position: fixed` popover to a trigger element →
 *    `anchorRect(el)`
 *  - clamping either to the screen edge → `layoutViewport()`
 *  - mapping the cursor into a grid / drag surface →
 *    `pointerOffsetIn(e, el)`
 *  - converting a drag *delta* (document-level pointermove) →
 *    `visualDeltaToLayout(dy)`
 */

/** The minimal element surface the ratio measurement needs — kept
 *  structural so node-side unit tests can pass plain stub objects
 *  (jsdom cannot emulate CSS zoom, so the formulae are tested with
 *  fabricated sizes instead). */
export interface SizeProbe {
  offsetWidth: number
  offsetHeight: number
  getBoundingClientRect(): { left: number; top: number; width: number; height: number }
}

/** A point in cursor/event coordinates.  Structural subset of
 *  MouseEvent / PointerEvent so tests can pass plain objects. */
export interface ClientPoint {
  clientX: number
  clientY: number
}

function rootProbe(): SizeProbe {
  return document.documentElement
}

/**
 * Multiply a visual-pixel measurement by this to get layout pixels
 * in `el`'s coordinate space (default: the document root, i.e. the
 * space `position: fixed` coordinates live in).
 *
 * Measured off the larger of the element's two dimensions —
 * `offsetWidth/Height` round to whole pixels, so the bigger side
 * carries the smaller relative rounding error.
 */
export function visualToLayoutRatio(el: SizeProbe = rootProbe()): number {
  const rect = el.getBoundingClientRect()
  const useHeight = el.offsetHeight >= el.offsetWidth
  const layout = useHeight ? el.offsetHeight : el.offsetWidth
  const visual = useHeight ? rect.height : rect.width
  return layout > 0 && visual > 0 ? layout / visual : 1
}

/**
 * Where to place a `position: fixed` overlay so it appears at the
 * cursor — the canonical anchor for right-click context menus.
 */
export function cursorAnchor(
  e: ClientPoint,
  root: SizeProbe = rootProbe(),
): { x: number; y: number } {
  const ratio = visualToLayoutRatio(root)
  return { x: e.clientX * ratio, y: e.clientY * ratio }
}

/**
 * A trigger element's bounding box translated into `position:
 * fixed` coordinate space — the canonical anchor for three-dot
 * menus, popovers and anything else that opens *next to* an
 * element rather than at the cursor.
 */
export function anchorRect(
  el: SizeProbe,
  root: SizeProbe = rootProbe(),
): { left: number; top: number; right: number; bottom: number; width: number; height: number } {
  const ratio = visualToLayoutRatio(root)
  const r = el.getBoundingClientRect()
  return {
    left: r.left * ratio,
    top: r.top * ratio,
    right: (r.left + r.width) * ratio,
    bottom: (r.top + r.height) * ratio,
    width: r.width * ratio,
    height: r.height * ratio,
  }
}

/**
 * The viewport size in `position: fixed` coordinate space — use
 * this (never `window.innerWidth/Height`, which are visual pixels)
 * when clamping a menu or popover to the screen edge.
 */
export function layoutViewport(
  root: SizeProbe = rootProbe(),
  win: { innerWidth: number; innerHeight: number } = window,
): { width: number; height: number } {
  const ratio = visualToLayoutRatio(root)
  return { width: win.innerWidth * ratio, height: win.innerHeight * ratio }
}

/**
 * Keep a fixed-position overlay on screen: pull the anchor back
 * from the right/bottom edges by the overlay's estimated size.
 * Inputs and output are in layout (fixed-position) space — i.e.
 * feed it `cursorAnchor(...)` / `anchorRect(...)` values, never raw
 * client coordinates.
 */
export function clampToViewport(
  pos: { x: number; y: number },
  estWidth: number,
  estHeight: number,
  root: SizeProbe = rootProbe(),
  win: { innerWidth: number; innerHeight: number } = window,
): { x: number; y: number } {
  const vp = layoutViewport(root, win)
  return {
    x: Math.max(0, Math.min(pos.x, vp.width - estWidth)),
    y: Math.max(0, Math.min(pos.y, vp.height - estHeight)),
  }
}

/**
 * The cursor's offset *inside* `el`, in `el`'s own layout pixels —
 * the space px-per-minute grid constants and absolutely-positioned
 * children are expressed in.  Use for click/drag maths on grids
 * and other interactive surfaces.
 */
export function pointerOffsetIn(
  e: ClientPoint,
  el: SizeProbe,
): { x: number; y: number } {
  const ratio = visualToLayoutRatio(el)
  const rect = el.getBoundingClientRect()
  return { x: (e.clientX - rect.left) * ratio, y: (e.clientY - rect.top) * ratio }
}

/**
 * Convert a drag delta (difference of two client coordinates, e.g.
 * from a document-level `pointermove` loop) into layout pixels.
 * Without this, sidebar resizes and drag-reschedules move
 * faster/slower than the cursor while zoomed.
 */
export function visualDeltaToLayout(
  deltaPx: number,
  root: SizeProbe = rootProbe(),
): number {
  return deltaPx * visualToLayoutRatio(root)
}

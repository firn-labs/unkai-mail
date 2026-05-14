// Drag-to-resize for vertical sidebars (#288).
//
// Applied as a Svelte action on any element that should function as
// a resizable left sidebar:
//
//     <aside class="shrink-0 border-r ... flex flex-col"
//            use:resizableSidebar={{
//              key: 'mail.folderSidebar',
//              defaultWidth: 224,
//              min: 160,
//              max: 480,
//            }}>
//
// The action takes over the element's `width` and `position` inline
// styles, appends an absolutely-positioned drag handle on the right
// edge, and persists the user's chosen width to localStorage under
// `nimbus.resize.<key>`.  On the next mount the saved width is read
// back and clamped to the current [min, max] bounds.
//
// IMPORTANT: don't keep a tailwind `w-*` class on the host element
// when this action is used — the inline style would still win, but
// removing the class avoids a one-frame flash to the wrong width
// before mount.  Keep `shrink-0` so the element doesn't get squeezed
// when its siblings demand space.
//
// The handle is 4px wide visually, but the hit-target extends 4px
// either side via negative offsets + transparent padding so users
// don't have to be pixel-precise.  Hover and active states fade the
// primary-500 accent in, matching the rest of the chrome.

import { m } from '../paraglide/messages'

const STORAGE_PREFIX = 'nimbus.resize.'

export interface ResizableSidebarOptions {
  /** Stable identifier — becomes `nimbus.resize.<key>` in localStorage.
   *  Pick one that won't collide across sidebars (e.g. `mail.folderSidebar`,
   *  `notes.notesList`). */
  key: string
  /** Width applied when no saved value exists yet, in CSS pixels. */
  defaultWidth: number
  /** Lower clamp.  Defaults to 140px — narrow enough for an icon-only
   *  collapse but wide enough that a single Tailwind `w-1` border
   *  doesn't dominate the column. */
  min?: number
  /** Upper clamp.  Defaults to 600px — past that the sidebar starts
   *  eating real estate the main pane needs. */
  max?: number
}

interface State {
  options: Required<ResizableSidebarOptions>
  handle: HTMLDivElement
  onPointerDown: (e: PointerEvent) => void
  cleanupDrag: (() => void) | null
}

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value))
}

function readPersisted(key: string): number | null {
  try {
    const raw = localStorage.getItem(STORAGE_PREFIX + key)
    if (raw === null) return null
    const n = Number.parseFloat(raw)
    return Number.isFinite(n) ? n : null
  } catch {
    return null
  }
}

function writePersisted(key: string, width: number): void {
  try {
    localStorage.setItem(STORAGE_PREFIX + key, String(Math.round(width)))
  } catch {
    /* storage unavailable — silent */
  }
}

function applyDefaults(o: ResizableSidebarOptions): Required<ResizableSidebarOptions> {
  return {
    key: o.key,
    defaultWidth: o.defaultWidth,
    min: o.min ?? 140,
    max: o.max ?? 600,
  }
}

function buildHandle(): HTMLDivElement {
  const handle = document.createElement('div')
  handle.setAttribute('role', 'separator')
  handle.setAttribute('aria-orientation', 'vertical')
  handle.setAttribute('aria-label', m.chrome_sidebar_resize_handle_label())
  // The visible 4px bar sits flush with the right edge; the
  // transparent padding extends the pointer hit-target 4px in
  // either direction so the user doesn't have to land on a hairline.
  handle.style.cssText = [
    'position: absolute',
    'top: 0',
    'bottom: 0',
    'right: -4px',
    'width: 8px',
    'cursor: col-resize',
    'z-index: 20',
    'touch-action: none',
    'user-select: none',
    // The accent stripe is a 4px inner span we paint via a CSS
    // pseudo using ::before-style — but inside inline styles we
    // fake it with a box-shadow inset on hover via a hover handler.
    'background: transparent',
    'transition: background-color 120ms ease',
  ].join(';')
  // Hover / active states: tint the right 4px slice.  Done with
  // direct event handlers since we can't attach `:hover` rules
  // through inline styles.
  const setActive = (active: boolean) => {
    handle.style.backgroundImage = active
      ? 'linear-gradient(to right, transparent 0, transparent 4px, var(--color-primary-500, #6366f1) 4px, var(--color-primary-500, #6366f1) 8px)'
      : 'none'
  }
  handle.addEventListener('mouseenter', () => setActive(true))
  handle.addEventListener('mouseleave', () => {
    if (!handle.dataset.dragging) setActive(false)
  })
  return handle
}

export function resizableSidebar(
  el: HTMLElement,
  rawOptions: ResizableSidebarOptions,
): { update(next: ResizableSidebarOptions): void; destroy(): void } {
  const state: State = {
    options: applyDefaults(rawOptions),
    handle: buildHandle(),
    onPointerDown: () => {},
    cleanupDrag: null,
  }

  const ensurePositioned = () => {
    const computed = getComputedStyle(el).position
    if (computed === 'static') el.style.position = 'relative'
  }

  const applyWidth = (w: number) => {
    const clamped = clamp(w, state.options.min, state.options.max)
    el.style.width = `${clamped}px`
    el.style.flexShrink = '0'
    return clamped
  }

  // Initial width: persisted value if any, otherwise the caller's
  // default.  Either way we clamp so a stale stored value from an
  // older min/max never escapes the new bounds.
  const persisted = readPersisted(state.options.key)
  applyWidth(persisted ?? state.options.defaultWidth)

  ensurePositioned()
  el.appendChild(state.handle)

  state.onPointerDown = (e: PointerEvent) => {
    if (e.button !== 0) return
    e.preventDefault()
    e.stopPropagation()
    state.handle.dataset.dragging = 'true'
    state.handle.setPointerCapture(e.pointerId)

    const startX = e.clientX
    const startWidth = el.getBoundingClientRect().width

    // Lock cursor + selection globally so dragging across other
    // panes doesn't snap to a text-select cursor mid-drag.
    const prevBodyCursor = document.body.style.cursor
    const prevBodyUserSelect = document.body.style.userSelect
    document.body.style.cursor = 'col-resize'
    document.body.style.userSelect = 'none'

    const onMove = (ev: PointerEvent) => {
      const dx = ev.clientX - startX
      applyWidth(startWidth + dx)
    }
    const onUp = () => {
      const finalWidth = el.getBoundingClientRect().width
      writePersisted(state.options.key, finalWidth)
      teardown()
    }
    const teardown = () => {
      document.removeEventListener('pointermove', onMove)
      document.removeEventListener('pointerup', onUp)
      document.removeEventListener('pointercancel', onUp)
      document.body.style.cursor = prevBodyCursor
      document.body.style.userSelect = prevBodyUserSelect
      delete state.handle.dataset.dragging
      state.handle.style.backgroundImage = 'none'
      state.cleanupDrag = null
    }
    state.cleanupDrag = teardown

    document.addEventListener('pointermove', onMove)
    document.addEventListener('pointerup', onUp)
    document.addEventListener('pointercancel', onUp)
  }

  state.handle.addEventListener('pointerdown', state.onPointerDown)

  return {
    update(next: ResizableSidebarOptions) {
      state.options = applyDefaults(next)
      // Re-clamp the current width to the (possibly new) bounds.
      applyWidth(el.getBoundingClientRect().width)
    },
    destroy() {
      state.cleanupDrag?.()
      state.handle.removeEventListener('pointerdown', state.onPointerDown)
      state.handle.remove()
    },
  }
}

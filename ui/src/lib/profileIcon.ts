/**
 * profileIcon — the one module that turns a profile's identity
 * (emoji or named icon, #530) into window chrome (#536).
 *
 * Two halves, one home:
 *
 *   - DOM rendering of the glyph itself lives in the companion
 *     `ProfileGlyph.svelte` (rail bubble, switcher popover rows,
 *     the transition screen).  Components never branch on
 *     `icon.kind` themselves.
 *   - Canvas work lives here: compositing the profile badge onto
 *     the app logo for the per-window taskbar icon, plus the
 *     window-title formatting that rides in the same
 *     `set_window_identity` IPC.
 *
 * The compositing happens in the FRONTEND on purpose: the webview
 * rasterizes colour emoji natively (canvas `fillText`), which Rust
 * cannot do without dragging in a font stack.  The backend only
 * receives finished PNG bytes and calls the window's `set_icon`.
 *
 * Node-importable: the DOM/Svelte pieces are loaded lazily inside
 * `renderWindowIcon`, so the pure helpers stay unit-testable under
 * vitest's node environment.
 */

import type { Profile, ProfileIcon } from './api'

export const APP_NAME = 'Unkai Mail'

/**
 * The window title carrying the profile identity: "Unkai Mail —
 * Work", with the emoji prefixed when the profile has one ("🦊
 * Unkai Mail — Work") — titles are the one surface every platform
 * honours, so the emoji rides there too.  Single-profile installs
 * keep the plain app name: "Unkai Mail — Default" on every window
 * would be noise for the common case.
 */
export function profileWindowTitle(
  profile: Pick<Profile, 'name' | 'icon'>,
  multiProfile: boolean,
): string {
  if (!multiProfile) return APP_NAME
  const base = `${APP_NAME} — ${profile.name}`
  if (profile.icon.kind === 'emoji' && profile.icon.value.trim()) {
    return `${profile.icon.value.trim()} ${base}`
  }
  return base
}

/** Side length of the composited icon — matches the 256px logo
 *  assets the `unkai-logo://` scheme serves. */
const ICON_SIZE = 256

/** Badge circle: bottom-right, half the icon's width — small
 *  enough to keep the logo recognisable, big enough to survive the
 *  taskbar's ~32px downscale. */
const BADGE_RADIUS = 64
const BADGE_CENTER = ICON_SIZE - BADGE_RADIUS - 4

/**
 * Composite the app logo with the profile's badge and return the
 * PNG bytes for `set_window_identity`, or the plain logo when
 * `badge` is null (single-profile installs — also what restores a
 * window after the second-to-last profile is deleted).
 *
 * Resolves to `null` when the environment can't produce the icon
 * (canvas unavailable, logo failed to load) — callers skip the
 * icon and still set the title.
 */
export async function renderWindowIcon(
  baseLogoUrl: string,
  badge: ProfileIcon | null,
): Promise<number[] | null> {
  let base: HTMLImageElement
  try {
    base = await loadImage(baseLogoUrl)
  } catch (e) {
    console.warn('window icon: base logo failed to load', e)
    return null
  }
  const canvas = document.createElement('canvas')
  canvas.width = ICON_SIZE
  canvas.height = ICON_SIZE
  const ctx = canvas.getContext('2d')
  if (!ctx) return null
  ctx.drawImage(base, 0, 0, ICON_SIZE, ICON_SIZE)

  if (badge) {
    if (badge.kind === 'emoji' && badge.value.trim()) {
      drawBadgeCircle(ctx, badgeSurfaceColor())
      // Emoji fonts put extra ascent above the visible glyph, so a
      // middle-baseline draw sits low — the small upward nudge is
      // the canvas twin of the rail bubble's 2px translate.
      ctx.font =
        `${Math.round(BADGE_RADIUS * 1.4)}px ` +
        '"Segoe UI Emoji", "Apple Color Emoji", "Noto Color Emoji", sans-serif'
      ctx.textAlign = 'center'
      ctx.textBaseline = 'middle'
      ctx.fillText(
        badge.value.trim(),
        BADGE_CENTER,
        BADGE_CENTER + Math.round(BADGE_RADIUS * 0.08),
      )
    } else {
      // Named icon: tonal primary disc + white glyph, the badge
      // twin of the rail's primary-tinted profile bubble.
      drawBadgeCircle(ctx, badgePrimaryColor())
      const glyph = await loadNamedIconImage(badge.value, Math.round(BADGE_RADIUS * 1.15))
      if (glyph) {
        const s = Math.round(BADGE_RADIUS * 1.15)
        ctx.drawImage(glyph, BADGE_CENTER - s / 2, BADGE_CENTER - s / 2, s, s)
      }
    }
  }

  const blob = await new Promise<Blob | null>((resolve) =>
    canvas.toBlob(resolve, 'image/png'),
  )
  if (!blob) return null
  return Array.from(new Uint8Array(await blob.arrayBuffer()))
}

/** White-ringed disc the badge content sits on — keeps both emoji
 *  and glyph legible over whatever logo style is underneath. */
function drawBadgeCircle(ctx: CanvasRenderingContext2D, fill: string) {
  ctx.beginPath()
  ctx.arc(BADGE_CENTER, BADGE_CENTER, BADGE_RADIUS, 0, Math.PI * 2)
  ctx.fillStyle = fill
  ctx.fill()
  ctx.lineWidth = 6
  ctx.strokeStyle = 'rgba(255, 255, 255, 0.9)'
  ctx.stroke()
}

/** Near-white disc for emoji badges.  A fixed colour, not a theme
 *  token: the taskbar icon lives outside the app's theme and must
 *  read the same in light and dark shells. */
function badgeSurfaceColor(): string {
  return '#f4f5f7'
}

/**
 * Primary-tinted disc for named-icon badges.  The theme variable
 * is passed through VERBATIM to the canvas (browsers parse oklch;
 * we never hand-parse theme colours — see CLAUDE.md) with a fixed
 * fallback for environments where the variable isn't set (e.g. a
 * detached test DOM).
 */
function badgePrimaryColor(): string {
  const fallback = '#5b7c99'
  if (typeof document === 'undefined') return fallback
  const v = getComputedStyle(document.documentElement)
    .getPropertyValue('--color-primary-500')
    .trim()
  return v || fallback
}

/**
 * Rasterize a named icon from the shared `Icon.svelte` registry —
 * mounted into a detached element so the icon art has exactly one
 * source, then re-loaded as an SVG image for `drawImage`.  Lazy
 * imports keep this module importable under node (vitest).
 */
async function loadNamedIconImage(
  name: string,
  size: number,
): Promise<HTMLImageElement | null> {
  try {
    const [{ mount, unmount }, { default: Icon }] = await Promise.all([
      import('svelte'),
      import('./Icon.svelte'),
    ])
    const host = document.createElement('div')
    const instance = mount(Icon, { target: host, props: { name: name as never, size } })
    const svg = host.querySelector('svg')
    let markup: string | null = null
    if (svg) {
      // `currentColor` strokes resolve from the CSS `color`
      // property; an SVG loaded as an <img> has no cascade, so the
      // colour must ride inline.
      svg.style.color = '#ffffff'
      markup = svg.outerHTML
    }
    await unmount(instance)
    if (!markup) return null
    return await loadImage(
      `data:image/svg+xml;charset=utf-8,${encodeURIComponent(markup)}`,
    )
  } catch (e) {
    console.warn('window icon: named-icon rasterization failed', e)
    return null
  }
}

function loadImage(src: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const img = new Image()
    // The base logo comes from the `unkai-logo://` scheme — a
    // different origin than the app — and this image is drawn
    // onto a canvas we read back with `toBlob()`.  Without an
    // anonymous-CORS load (paired with the ACAO header the logo
    // protocol sends) the canvas is tainted and the readback
    // throws, which is exactly a silent "title works, icon
    // doesn't" failure.  Harmless for the data: URI glyphs.
    img.crossOrigin = 'anonymous'
    img.onload = () => resolve(img)
    img.onerror = () => reject(new Error(`image failed to load: ${src}`))
    img.src = src
  })
}

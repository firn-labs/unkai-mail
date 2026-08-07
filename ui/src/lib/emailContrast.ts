/**
 * Mail-body text contrast on the app-theme background (#472).
 *
 * HTML emails are written against an assumed white page: senders set
 * `color: #333` (or leave the background out entirely) and rely on the
 * client to supply the light canvas. The "white background" reading
 * mode does exactly that. But in "use the app's theme" mode the canvas
 * is whatever the active Skeleton theme says — near-black in dark mode
 * — and those inline near-black text colours vanish into it.
 *
 * This module walks the sanitised body and re-tints only the inline
 * colours that would be unreadable against the *effective* background
 * behind them (the app canvas, or any background the email itself
 * brings along — a white table cell keeps its dark text). Adjustment
 * preserves hue and saturation and only moves lightness until the
 * WCAG contrast ratio clears the readability floor, so a dark-blue
 * link becomes a light blue rather than plain white.
 *
 * Kept out of MailView so the colour math stays unit-testable without
 * a DOM (the vitest setup is node-only); only `ensureReadableEmailText`
 * touches elements.
 */

/** sRGB colour, channels 0–255, alpha 0–1. */
export interface Rgb {
  r: number
  g: number
  b: number
  a: number
}

/** Minimum WCAG contrast ratio before we re-tint a colour. 4.5:1 is
 *  the AA floor for normal-size text — the same bar the app's own
 *  `--text-on-glass` tokens are calibrated against (#453). */
export const MIN_CONTRAST = 4.5

/* ── Parsing ─────────────────────────────────────────────────────────── */

/** Named colours that actually show up in real mail bodies — the CSS2
 *  basic set plus the grey ladder and a few newsletter favourites.
 *  Anything rarer falls through to the caller's `resolveColor`
 *  fallback (a canvas probe in the reader), so this map only has to
 *  cover the common cases, not the full 148-name CSS registry. */
const NAMED_COLORS: Record<string, [number, number, number]> = {
  black: [0, 0, 0],
  white: [255, 255, 255],
  red: [255, 0, 0],
  green: [0, 128, 0],
  blue: [0, 0, 255],
  yellow: [255, 255, 0],
  orange: [255, 165, 0],
  purple: [128, 0, 128],
  gray: [128, 128, 128],
  grey: [128, 128, 128],
  silver: [192, 192, 192],
  maroon: [128, 0, 0],
  navy: [0, 0, 128],
  teal: [0, 128, 128],
  olive: [128, 128, 0],
  aqua: [0, 255, 255],
  cyan: [0, 255, 255],
  fuchsia: [255, 0, 255],
  magenta: [255, 0, 255],
  lime: [0, 255, 0],
  dimgray: [105, 105, 105],
  dimgrey: [105, 105, 105],
  darkgray: [169, 169, 169],
  darkgrey: [169, 169, 169],
  lightgray: [211, 211, 211],
  lightgrey: [211, 211, 211],
  gainsboro: [220, 220, 220],
  whitesmoke: [245, 245, 245],
  darkblue: [0, 0, 139],
  darkred: [139, 0, 0],
  darkgreen: [0, 100, 0],
  darkslategray: [47, 79, 79],
  darkslategrey: [47, 79, 79],
  slategray: [112, 128, 144],
  slategrey: [112, 128, 144],
  lightslategray: [119, 136, 153],
  lightslategrey: [119, 136, 153],
  midnightblue: [25, 25, 112],
  royalblue: [65, 105, 225],
  dodgerblue: [30, 144, 255],
  steelblue: [70, 130, 180],
  crimson: [220, 20, 60],
  indigo: [75, 0, 130],
  brown: [165, 42, 42],
  chocolate: [210, 105, 30],
  gold: [255, 215, 0],
  ivory: [255, 255, 240],
  beige: [245, 245, 220],
  tomato: [255, 99, 71],
  coral: [255, 127, 80],
  salmon: [250, 128, 114],
  khaki: [240, 230, 140],
  lavender: [230, 230, 250],
  plum: [221, 160, 221],
  orchid: [218, 112, 214],
  violet: [238, 130, 238],
  pink: [255, 192, 203],
  hotpink: [255, 105, 180],
  skyblue: [135, 206, 235],
  lightblue: [173, 216, 230],
  lightskyblue: [135, 206, 250],
  cornflowerblue: [100, 149, 237],
  seagreen: [46, 139, 87],
  forestgreen: [34, 139, 34],
  limegreen: [50, 205, 50],
  darkorange: [255, 140, 0],
  orangered: [255, 69, 0],
  firebrick: [178, 34, 34],
  goldenrod: [218, 165, 32],
  tan: [210, 180, 140],
  wheat: [245, 222, 179],
  snow: [255, 250, 250],
  linen: [250, 240, 230],
  aliceblue: [240, 248, 255],
  ghostwhite: [248, 248, 255],
  mintcream: [245, 255, 250],
  honeydew: [240, 255, 240],
  floralwhite: [255, 250, 240],
  transparent: [0, 0, 0],
}

/**
 * Parse the colour syntaxes that occur in real email inline styles:
 * hex (#rgb/#rgba/#rrggbb/#rrggbbaa), rgb()/rgba(), hsl()/hsla(), and
 * common named colours. Returns null for anything it doesn't
 * recognise (var(), gradients, `inherit`, exotic spaces) — callers
 * treat null as "leave this element alone" or route it through their
 * own resolver.
 */
export function parseCssColor(raw: string | null | undefined): Rgb | null {
  if (!raw) return null
  const value = raw.trim().toLowerCase()
  if (!value) return null

  if (value === 'transparent') return { r: 0, g: 0, b: 0, a: 0 }
  const named = NAMED_COLORS[value]
  if (named) return { r: named[0], g: named[1], b: named[2], a: 1 }

  if (value.startsWith('#')) return parseHex(value.slice(1))

  const fn = value.match(/^(rgba?|hsla?)\(\s*([^)]+)\)$/)
  if (!fn) return null
  const parts = fn[2]
    .split(/[\s,/]+/)
    .map((p) => p.trim())
    .filter(Boolean)
  if (parts.length < 3) return null

  if (fn[1].startsWith('rgb')) {
    const chan = (p: string): number | null => {
      if (p.endsWith('%')) {
        const v = Number(p.slice(0, -1))
        return Number.isFinite(v) ? clamp((v / 100) * 255, 0, 255) : null
      }
      const v = Number(p)
      return Number.isFinite(v) ? clamp(v, 0, 255) : null
    }
    const r = chan(parts[0])
    const g = chan(parts[1])
    const b = chan(parts[2])
    if (r == null || g == null || b == null) return null
    return { r, g, b, a: parseAlpha(parts[3]) }
  }

  const h = Number(parts[0].replace(/deg$/, ''))
  const s = Number(parts[1].replace(/%$/, ''))
  const l = Number(parts[2].replace(/%$/, ''))
  if (!Number.isFinite(h) || !Number.isFinite(s) || !Number.isFinite(l)) return null
  const [r, g, b] = hslToRgb(((h % 360) + 360) % 360, clamp(s, 0, 100) / 100, clamp(l, 0, 100) / 100)
  return { r, g, b, a: parseAlpha(parts[3]) }
}

function parseHex(hex: string): Rgb | null {
  if (!/^[0-9a-f]+$/.test(hex)) return null
  if (hex.length === 3 || hex.length === 4) {
    const [r, g, b, a] = hex.split('').map((c) => parseInt(c + c, 16))
    return { r, g, b, a: hex.length === 4 ? a / 255 : 1 }
  }
  if (hex.length === 6 || hex.length === 8) {
    const r = parseInt(hex.slice(0, 2), 16)
    const g = parseInt(hex.slice(2, 4), 16)
    const b = parseInt(hex.slice(4, 6), 16)
    const a = hex.length === 8 ? parseInt(hex.slice(6, 8), 16) / 255 : 1
    return { r, g, b, a }
  }
  return null
}

function parseAlpha(p: string | undefined): number {
  if (p == null) return 1
  const v = p.endsWith('%') ? Number(p.slice(0, -1)) / 100 : Number(p)
  return Number.isFinite(v) ? clamp(v, 0, 1) : 1
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v))
}

/* ── Colour math ─────────────────────────────────────────────────────── */

/** WCAG 2.x relative luminance (0 = black, 1 = white). */
export function relativeLuminance(c: Rgb): number {
  const lin = (v: number) => {
    const s = v / 255
    return s <= 0.04045 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4)
  }
  return 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/** WCAG contrast ratio between two opaque colours, 1–21. */
export function contrastRatio(a: Rgb, b: Rgb): number {
  const la = relativeLuminance(a)
  const lb = relativeLuminance(b)
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05)
}

/** Alpha-composite `fg` over an opaque `bg`. */
export function blendOver(fg: Rgb, bg: Rgb): Rgb {
  if (fg.a >= 1) return { ...fg, a: 1 }
  const mix = (f: number, b: number) => Math.round(f * fg.a + b * (1 - fg.a))
  return { r: mix(fg.r, bg.r), g: mix(fg.g, bg.g), b: mix(fg.b, bg.b), a: 1 }
}

function rgbToHsl(c: Rgb): [number, number, number] {
  const r = c.r / 255
  const g = c.g / 255
  const b = c.b / 255
  const max = Math.max(r, g, b)
  const min = Math.min(r, g, b)
  const l = (max + min) / 2
  if (max === min) return [0, 0, l]
  const d = max - min
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min)
  let h: number
  if (max === r) h = ((g - b) / d + (g < b ? 6 : 0)) / 6
  else if (max === g) h = ((b - r) / d + 2) / 6
  else h = ((r - g) / d + 4) / 6
  return [h * 360, s, l]
}

function hslToRgb(h: number, s: number, l: number): [number, number, number] {
  if (s === 0) {
    const v = Math.round(l * 255)
    return [v, v, v]
  }
  const q = l < 0.5 ? l * (1 + s) : l + s - l * s
  const p = 2 * l - q
  const hue = (t: number): number => {
    if (t < 0) t += 1
    if (t > 1) t -= 1
    if (t < 1 / 6) return p + (q - p) * 6 * t
    if (t < 1 / 2) return q
    if (t < 2 / 3) return p + (q - p) * (2 / 3 - t) * 6
    return p
  }
  const hn = h / 360
  return [
    Math.round(hue(hn + 1 / 3) * 255),
    Math.round(hue(hn) * 255),
    Math.round(hue(hn - 1 / 3) * 255),
  ]
}

/**
 * Re-tint `text` so it clears `MIN_CONTRAST` against the opaque `bg`,
 * or return null when it's already readable. Hue and saturation are
 * kept; lightness walks toward whichever pole (white / black) can
 * actually deliver contrast on this background, stopping at the first
 * step that clears the floor. If even the pole can't clear it (a
 * mid-grey background), the pole is returned — that's the best
 * available.
 */
export function readableTextColor(text: Rgb, bg: Rgb): Rgb | null {
  const composited = text.a < 1 ? blendOver(text, bg) : { ...text, a: 1 }
  if (contrastRatio(composited, bg) >= MIN_CONTRAST) {
    // Translucent-but-readable stays untouched; fully covered by the
    // "already readable" contract.
    return null
  }

  const white: Rgb = { r: 255, g: 255, b: 255, a: 1 }
  const black: Rgb = { r: 0, g: 0, b: 0, a: 1 }
  const towardWhite = contrastRatio(white, bg) >= contrastRatio(black, bg)

  const [h, s, l] = rgbToHsl(composited)
  const step = 0.05
  for (
    let cand = l;
    towardWhite ? cand <= 1 : cand >= 0;
    cand = towardWhite ? cand + step : cand - step
  ) {
    const [r, g, b] = hslToRgb(h, s, clamp(cand, 0, 1))
    const rgb: Rgb = { r, g, b, a: 1 }
    if (contrastRatio(rgb, bg) >= MIN_CONTRAST) return rgb
  }
  return towardWhite ? white : black
}

export function formatRgb(c: Rgb): string {
  return `rgb(${Math.round(c.r)}, ${Math.round(c.g)}, ${Math.round(c.b)})`
}

/* ── DOM pass ────────────────────────────────────────────────────────── */

export interface ContrastContext {
  /** Opaque colour of the canvas the mail body sits on (the app
   *  theme's page background in "use mail theme" mode). */
  background: Rgb
  /** Colour un-styled text inherits from the app (the theme's base
   *  font colour). */
  text: Rgb
  /** Optional escape hatch for colour syntaxes `parseCssColor`
   *  doesn't know (the reader wires a canvas probe here). Return
   *  null to leave the element untouched. */
  resolveColor?: (value: string) => Rgb | null
}

/**
 * Walk a sanitised mail body and re-tint inline text colours that
 * would be unreadable against the effective background behind them.
 *
 * Two situations get fixed, both tracked by carrying the effective
 * (background, inherited-text) pair down the tree:
 *  - an element sets an unreadable inline `color` (or `<font color>`)
 *    → the colour is re-tinted in place;
 *  - an element brings its own background (inline `background-color`
 *    or legacy `bgcolor=`) that clashes with the text colour it
 *    inherits — e.g. a white table cell relying on "clients default
 *    to black text" while the dark theme inherits near-white — → the
 *    element gets an explicit readable colour stamped on it.
 *
 * Elements whose colours already clear the floor are left exactly as
 * the sender wrote them; backgrounds are never altered.
 */
export function ensureReadableEmailText(root: Element, ctx: ContrastContext): void {
  const parse = (value: string | null): Rgb | null => {
    if (!value) return null
    return parseCssColor(value) ?? ctx.resolveColor?.(value) ?? null
  }
  walk(root, ctx.background, ctx.text, parse)
}

function walk(
  el: Element,
  bg: Rgb,
  inheritedText: Rgb,
  parse: (value: string | null) => Rgb | null,
): void {
  const style = (el as HTMLElement).style as CSSStyleDeclaration | undefined

  // Effective background for this subtree. `style.backgroundColor`
  // also surfaces the colour component of a `background:` shorthand
  // via CSSOM decomposition; `bgcolor` is the table-era attribute
  // newsletters still carry.
  const ownBg = parse(style?.backgroundColor || null) ?? parse(el.getAttribute('bgcolor'))
  if (ownBg && ownBg.a > 0) bg = blendOver(ownBg, bg)

  const ownColor =
    parse(style?.color || null) ??
    (el.tagName === 'FONT' ? parse(el.getAttribute('color')) : null)

  if (ownColor) {
    const fixed = readableTextColor(ownColor, bg)
    if (fixed) {
      // Setting the inline style also outranks a `<font color>` attr,
      // so one write covers both sources.
      ;(el as HTMLElement).style.setProperty('color', formatRgb(fixed))
      inheritedText = fixed
    } else {
      inheritedText = ownColor.a < 1 ? blendOver(ownColor, bg) : ownColor
    }
  } else if (ownBg && ownBg.a > 0) {
    // The element brought its own canvas but relies on inherited text.
    const fixed = readableTextColor(inheritedText, bg)
    if (fixed) {
      ;(el as HTMLElement).style.setProperty('color', formatRgb(fixed))
      inheritedText = fixed
    }
  }

  for (const child of Array.from(el.children)) walk(child, bg, inheritedText, parse)
}

import { describe, expect, it } from 'vitest'
import {
  MIN_CONTRAST,
  blendOver,
  contrastRatio,
  ensureReadableEmailText,
  formatRgb,
  parseCssColor,
  readableTextColor,
  relativeLuminance,
  type Rgb,
} from './emailContrast'

const WHITE: Rgb = { r: 255, g: 255, b: 255, a: 1 }
const BLACK: Rgb = { r: 0, g: 0, b: 0, a: 1 }
/** cerberus dark-mode page background — oklch(0.25 0 0) ≈ #303030. */
const DARK_BG: Rgb = { r: 48, g: 48, b: 48, a: 1 }

describe('parseCssColor', () => {
  it('parses 6-digit hex', () => {
    expect(parseCssColor('#336699')).toEqual({ r: 51, g: 102, b: 153, a: 1 })
  })

  it('parses 3-digit hex', () => {
    expect(parseCssColor('#369')).toEqual({ r: 51, g: 102, b: 153, a: 1 })
  })

  it('parses 8-digit hex alpha', () => {
    const c = parseCssColor('#33669980')
    expect(c).not.toBeNull()
    expect(c!.a).toBeCloseTo(0.5, 1)
  })

  it('parses rgb() with commas', () => {
    expect(parseCssColor('rgb(51, 102, 153)')).toEqual({ r: 51, g: 102, b: 153, a: 1 })
  })

  it('parses rgba() with alpha', () => {
    expect(parseCssColor('rgba(0, 0, 0, 0.87)')).toEqual({ r: 0, g: 0, b: 0, a: 0.87 })
  })

  it('parses space-separated rgb with slash alpha', () => {
    expect(parseCssColor('rgb(51 102 153 / 50%)')).toEqual({ r: 51, g: 102, b: 153, a: 0.5 })
  })

  it('parses percentage rgb channels', () => {
    expect(parseCssColor('rgb(100%, 0%, 0%)')).toEqual({ r: 255, g: 0, b: 0, a: 1 })
  })

  it('parses hsl()', () => {
    // hsl(0, 0%, 20%) is a pure grey at 20% lightness.
    expect(parseCssColor('hsl(0, 0%, 20%)')).toEqual({ r: 51, g: 51, b: 51, a: 1 })
  })

  it('parses named colours case-insensitively', () => {
    expect(parseCssColor('Black')).toEqual({ r: 0, g: 0, b: 0, a: 1 })
    expect(parseCssColor('NAVY')).toEqual({ r: 0, g: 0, b: 128, a: 1 })
  })

  it('parses transparent as zero alpha', () => {
    expect(parseCssColor('transparent')!.a).toBe(0)
  })

  it('returns null for values it cannot resolve', () => {
    expect(parseCssColor('var(--brand)')).toBeNull()
    expect(parseCssColor('inherit')).toBeNull()
    expect(parseCssColor('linear-gradient(red, blue)')).toBeNull()
    expect(parseCssColor('')).toBeNull()
    expect(parseCssColor(null)).toBeNull()
    expect(parseCssColor('#12345')).toBeNull()
    expect(parseCssColor('rgb(a, b, c)')).toBeNull()
  })
})

describe('relativeLuminance / contrastRatio', () => {
  it('anchors black and white', () => {
    expect(relativeLuminance(BLACK)).toBe(0)
    expect(relativeLuminance(WHITE)).toBeCloseTo(1, 5)
    expect(contrastRatio(BLACK, WHITE)).toBeCloseTo(21, 1)
  })

  it('is symmetric', () => {
    expect(contrastRatio(DARK_BG, WHITE)).toBeCloseTo(contrastRatio(WHITE, DARK_BG), 6)
  })

  it('matches the known #767676-on-white AA boundary', () => {
    const grey = parseCssColor('#767676')!
    expect(contrastRatio(grey, WHITE)).toBeGreaterThan(4.5)
    expect(contrastRatio(grey, WHITE)).toBeLessThan(4.6)
  })
})

describe('blendOver', () => {
  it('returns opaque colours unchanged', () => {
    expect(blendOver({ ...BLACK }, WHITE)).toEqual(BLACK)
  })

  it('composites translucent colours onto the background', () => {
    const half = blendOver({ r: 0, g: 0, b: 0, a: 0.5 }, WHITE)
    expect(half.r).toBe(128)
    expect(half.a).toBe(1)
  })
})

describe('readableTextColor', () => {
  it('leaves already-readable text untouched', () => {
    expect(readableTextColor(BLACK, WHITE)).toBeNull()
    expect(readableTextColor(WHITE, DARK_BG)).toBeNull()
  })

  it('lightens near-black text on a dark background', () => {
    const fixed = readableTextColor(parseCssColor('#1a1a1a')!, DARK_BG)
    expect(fixed).not.toBeNull()
    expect(contrastRatio(fixed!, DARK_BG)).toBeGreaterThanOrEqual(MIN_CONTRAST)
    // A grey stays a grey — no hue invented.
    expect(fixed!.r).toBe(fixed!.g)
    expect(fixed!.g).toBe(fixed!.b)
  })

  it('keeps hue when lightening a dark saturated colour', () => {
    const navy = parseCssColor('navy')!
    const fixed = readableTextColor(navy, DARK_BG)!
    expect(contrastRatio(fixed, DARK_BG)).toBeGreaterThanOrEqual(MIN_CONTRAST)
    // Still recognisably blue: blue channel dominates.
    expect(fixed.b).toBeGreaterThan(fixed.r)
    expect(fixed.b).toBeGreaterThan(fixed.g)
  })

  it('darkens unreadable light text on a light background', () => {
    const fixed = readableTextColor(parseCssColor('#eeeeee')!, WHITE)!
    expect(contrastRatio(fixed, WHITE)).toBeGreaterThanOrEqual(MIN_CONTRAST)
    expect(relativeLuminance(fixed)).toBeLessThan(relativeLuminance(parseCssColor('#eeeeee')!))
  })

  it('composites translucent text before judging contrast', () => {
    // Material-style rgba(0,0,0,.87) on a dark canvas is unreadable
    // even though the opaque base colour would parse as pure black.
    const fixed = readableTextColor({ r: 0, g: 0, b: 0, a: 0.87 }, DARK_BG)
    expect(fixed).not.toBeNull()
    expect(fixed!.a).toBe(1)
  })

  it('picks the reachable pole direction on a mid-grey background', () => {
    // On rgb(128,128,128) white tops out at ~3.9:1 but black reaches
    // ~5.3:1 — the walk must head down, not up, and still clear AA.
    const midGrey: Rgb = { r: 128, g: 128, b: 128, a: 1 }
    const fixed = readableTextColor({ r: 120, g: 120, b: 120, a: 1 }, midGrey)
    expect(fixed).not.toBeNull()
    expect(contrastRatio(fixed!, midGrey)).toBeGreaterThanOrEqual(MIN_CONTRAST)
    expect(relativeLuminance(fixed!)).toBeLessThan(relativeLuminance(midGrey))
  })
})

describe('formatRgb', () => {
  it('serialises to the legacy rgb() form inline styles accept', () => {
    expect(formatRgb({ r: 12.4, g: 200, b: 0, a: 1 })).toBe('rgb(12, 200, 0)')
  })
})

/**
 * The walker only touches a narrow structural surface — `style`
 * (`color` / `backgroundColor` reads, `setProperty` writes),
 * `getAttribute`, `tagName`, `children` — so a fake element covers it
 * in this node-only test setup without pulling in a DOM.
 */
class FakeElement {
  tagName: string
  style: { color: string; backgroundColor: string; setProperty(prop: string, v: string): void }
  children: FakeElement[]
  private attrs: Record<string, string>

  constructor(
    tagName: string,
    opts: { color?: string; backgroundColor?: string; attrs?: Record<string, string> } = {},
    children: FakeElement[] = [],
  ) {
    this.tagName = tagName.toUpperCase()
    this.children = children
    this.attrs = opts.attrs ?? {}
    const self = this
    this.style = {
      color: opts.color ?? '',
      backgroundColor: opts.backgroundColor ?? '',
      setProperty(prop: string, v: string) {
        if (prop === 'color') self.style.color = v
        if (prop === 'background-color') self.style.backgroundColor = v
      },
    }
  }

  getAttribute(name: string): string | null {
    return this.attrs[name] ?? null
  }

  asElement(): Element {
    return this as unknown as Element
  }
}

describe('ensureReadableEmailText', () => {
  const darkCtx = { background: DARK_BG, text: { r: 237, g: 237, b: 237, a: 1 } }

  it('re-tints near-black inline text sitting on the dark app canvas', () => {
    const p = new FakeElement('p', { color: '#222222' })
    ensureReadableEmailText(new FakeElement('body', {}, [p]).asElement(), darkCtx)
    const fixed = parseCssColor(p.style.color)!
    expect(contrastRatio(fixed, DARK_BG)).toBeGreaterThanOrEqual(MIN_CONTRAST)
  })

  it('leaves dark text alone inside a white cell the email brought', () => {
    const cell = new FakeElement('td', { backgroundColor: '#ffffff' }, [
      new FakeElement('p', { color: '#222222' }),
    ])
    ensureReadableEmailText(new FakeElement('body', {}, [cell]).asElement(), darkCtx)
    expect(cell.children[0].style.color).toBe('#222222')
  })

  it('stamps a readable colour on a white cell relying on inherited text', () => {
    // The email supplies a white bgcolor= but no text colour — the
    // theme's near-white inherited text would vanish on it.
    const cell = new FakeElement('td', { attrs: { bgcolor: '#ffffff' } })
    ensureReadableEmailText(new FakeElement('body', {}, [cell]).asElement(), darkCtx)
    const stamped = parseCssColor(cell.style.color)!
    expect(contrastRatio(stamped, WHITE)).toBeGreaterThanOrEqual(MIN_CONTRAST)
  })

  it('honours <font color> and overrides it via the inline style', () => {
    const font = new FakeElement('font', { attrs: { color: '#000000' } })
    ensureReadableEmailText(new FakeElement('body', {}, [font]).asElement(), darkCtx)
    const fixed = parseCssColor(font.style.color)!
    expect(contrastRatio(fixed, DARK_BG)).toBeGreaterThanOrEqual(MIN_CONTRAST)
  })

  it('judges descendants against their nearest effective background', () => {
    // Dark-designed email: dark wrapper with light text — readable as
    // shipped, so nothing may change even on a light app canvas.
    const p = new FakeElement('p', { color: '#f5f5f5' })
    const wrapper = new FakeElement('div', { backgroundColor: '#1c1c1c' }, [p])
    ensureReadableEmailText(new FakeElement('body', {}, [wrapper]).asElement(), {
      background: WHITE,
      text: { r: 24, g: 24, b: 24, a: 1 },
    })
    expect(p.style.color).toBe('#f5f5f5')
  })

  it('routes unknown colour syntax through resolveColor', () => {
    const p = new FakeElement('p', { color: 'oklch(0.2 0 0)' })
    ensureReadableEmailText(new FakeElement('body', {}, [p]).asElement(), {
      ...darkCtx,
      resolveColor: () => ({ r: 26, g: 26, b: 26, a: 1 }),
    })
    const fixed = parseCssColor(p.style.color)!
    expect(contrastRatio(fixed, DARK_BG)).toBeGreaterThanOrEqual(MIN_CONTRAST)
  })

  it('leaves elements with unresolvable colours untouched', () => {
    const p = new FakeElement('p', { color: 'var(--brand)' })
    ensureReadableEmailText(new FakeElement('body', {}, [p]).asElement(), darkCtx)
    expect(p.style.color).toBe('var(--brand)')
  })
})

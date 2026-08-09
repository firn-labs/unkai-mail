import { describe, expect, it } from 'vitest'
import {
  anchorRect,
  clampToViewport,
  cursorAnchor,
  layoutViewport,
  pointerOffsetIn,
  visualDeltaToLayout,
  visualToLayoutRatio,
  type SizeProbe,
} from './coords'

/** Fabricate an element whose layout box is `w×h` and whose visual
 *  box is scaled by `zoom` — jsdom cannot emulate CSS zoom, so the
 *  conversion formulae are exercised against stub geometry. */
function probe(w: number, h: number, zoom: number, left = 0, top = 0): SizeProbe {
  return {
    offsetWidth: w,
    offsetHeight: h,
    getBoundingClientRect: () => ({
      left: left * zoom,
      top: top * zoom,
      width: w * zoom,
      height: h * zoom,
    }),
  }
}

describe('visualToLayoutRatio', () => {
  it('is the identity at 100 % scale', () => {
    expect(visualToLayoutRatio(probe(1000, 700, 1))).toBe(1)
  })

  it('inverts the zoom factor', () => {
    expect(visualToLayoutRatio(probe(1000, 700, 1.25))).toBeCloseTo(0.8, 10)
    expect(visualToLayoutRatio(probe(1000, 700, 0.8))).toBeCloseTo(1.25, 10)
  })

  it('measures off the larger dimension', () => {
    // Height dominates → width can be degenerate without breaking it.
    expect(visualToLayoutRatio(probe(0, 1440, 1.5))).toBeCloseTo(1 / 1.5, 10)
  })

  it('falls back to 1 on degenerate geometry', () => {
    expect(visualToLayoutRatio(probe(0, 0, 1.25))).toBe(1)
  })
})

describe('cursorAnchor', () => {
  it('maps a click into fixed-position space', () => {
    const root = probe(1000, 700, 1.25)
    // Cursor at visual (250, 125) sits over layout point (200, 100).
    expect(cursorAnchor({ clientX: 250, clientY: 125 }, root)).toEqual({ x: 200, y: 100 })
  })
})

describe('anchorRect', () => {
  it('translates a trigger rect into fixed-position space', () => {
    const root = probe(1000, 700, 1.25)
    // Trigger at layout (80, 40), 32×16 — visual rect is ×1.25.
    const trigger = probe(32, 16, 1.25, 80, 40)
    const r = anchorRect(trigger, root)
    expect(r.left).toBeCloseTo(80, 10)
    expect(r.top).toBeCloseTo(40, 10)
    expect(r.right).toBeCloseTo(112, 10)
    expect(r.bottom).toBeCloseTo(56, 10)
    expect(r.width).toBeCloseTo(32, 10)
    expect(r.height).toBeCloseTo(16, 10)
  })
})

describe('layoutViewport', () => {
  it('shrinks the visual viewport by the zoom factor', () => {
    const root = probe(1000, 700, 1.25)
    const vp = layoutViewport(root, { innerWidth: 1250, innerHeight: 875 })
    expect(vp.width).toBeCloseTo(1000, 10)
    expect(vp.height).toBeCloseTo(700, 10)
  })
})

describe('clampToViewport', () => {
  const root = probe(1000, 700, 1.25)
  const win = { innerWidth: 1250, innerHeight: 875 }

  it('passes an on-screen anchor through untouched', () => {
    expect(clampToViewport({ x: 100, y: 100 }, 200, 150, root, win)).toEqual({ x: 100, y: 100 })
  })

  it('pulls the anchor back from the layout-space edges', () => {
    // Layout viewport is 1000×700 — clamp against that, not the
    // 1250×875 visual size.
    expect(clampToViewport({ x: 950, y: 680 }, 200, 150, root, win)).toEqual({ x: 800, y: 550 })
  })

  it('never goes negative on tiny windows', () => {
    expect(clampToViewport({ x: 10, y: 10 }, 2000, 2000, root, win)).toEqual({ x: 0, y: 0 })
  })
})

describe('pointerOffsetIn', () => {
  it('maps the cursor into the element’s own layout space', () => {
    // Day column 100 wide × 1440 tall at layout (300, 0), zoomed 1.25:
    // a cursor 125 visual px below the column top is 100 layout px in.
    const col = probe(100, 1440, 1.25, 300, 0)
    const p = pointerOffsetIn({ clientX: 400, clientY: 125 }, col)
    expect(p.x).toBeCloseTo(20, 10)
    expect(p.y).toBeCloseTo(100, 10)
  })

  it('is the plain rect offset at 100 % scale', () => {
    const col = probe(100, 1440, 1, 300, 0)
    expect(pointerOffsetIn({ clientX: 350, clientY: 60 }, col)).toEqual({ x: 50, y: 60 })
  })
})

describe('visualDeltaToLayout', () => {
  it('scales drag deltas into layout pixels', () => {
    expect(visualDeltaToLayout(50, probe(1000, 700, 1.25))).toBeCloseTo(40, 10)
    expect(visualDeltaToLayout(-50, probe(1000, 700, 1.25))).toBeCloseTo(-40, 10)
  })
})

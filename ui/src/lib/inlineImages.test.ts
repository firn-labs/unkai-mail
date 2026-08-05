import { describe, expect, test } from 'vitest'
import {
  buildInlineImageUrls,
  inlineImageKey,
  type InlineImagePart,
} from './inlineImages'

/** Minimal part factory — only the fields the matchers read. */
function part(over: Partial<InlineImagePart> = {}): InlineImagePart {
  return {
    partId: 0,
    contentId: null,
    filename: '',
    mime: 'image/png',
    base64: '',
    ...over,
  }
}

describe('inlineImageKey', () => {
  test('strips the cid: scheme', () => {
    expect(inlineImageKey('cid:logo@example')).toBe('logo@example')
  })

  test('strips the angle brackets senders copy from the header', () => {
    expect(inlineImageKey('<logo@example>')).toBe('logo@example')
    expect(inlineImageKey('cid:<logo@example>')).toBe('logo@example')
  })

  test('percent-decodes RFC 2392 URLs', () => {
    expect(inlineImageKey('cid:logo%40example.com')).toBe('logo@example.com')
  })

  test('compares case-insensitively', () => {
    expect(inlineImageKey('cid:Logo@Example')).toBe(inlineImageKey('LOGO@EXAMPLE'))
  })

  test('survives a malformed percent-escape', () => {
    // A filename like "50% off.png" is not valid percent-encoding;
    // keeping the raw form beats throwing away the image.
    expect(inlineImageKey('50% off.png')).toBe('50% off.png')
  })
})

describe('buildInlineImageUrls', () => {
  test('indexes a part by its content id', () => {
    const urls = buildInlineImageUrls(
      [part({ contentId: 'logo@example' })],
      () => 'blob:one',
    )
    expect(urls[inlineImageKey('cid:logo@example')]).toBe('blob:one')
  })

  test('also indexes by filename, for senders that reference the file', () => {
    const urls = buildInlineImageUrls(
      [part({ contentId: 'abc@x', filename: 'Logo.png' })],
      () => 'blob:one',
    )
    expect(urls['logo.png']).toBe('blob:one')
    expect(urls['abc@x']).toBe('blob:one')
  })

  test('creates one URL per part, not one per key', () => {
    let made = 0
    buildInlineImageUrls([part({ contentId: 'a@x', filename: 'a.png' })], () => {
      made += 1
      return `blob:${made}`
    })
    expect(made).toBe(1)
  })

  test('a content id is never shadowed by another part filename', () => {
    // Part 1 is referenced as cid:shared.png; part 2 happens to be
    // *named* shared.png.  The explicit Content-ID has to win.
    const urls = buildInlineImageUrls(
      [
        part({ partId: 0, contentId: 'shared.png' }),
        part({ partId: 1, filename: 'shared.png' }),
      ],
      (p) => `blob:${p.partId}`,
    )
    expect(urls['shared.png']).toBe('blob:0')
  })

  test('duplicate filenames resolve to the first part', () => {
    const urls = buildInlineImageUrls(
      [
        part({ partId: 0, filename: 'logo.png' }),
        part({ partId: 1, filename: 'logo.png' }),
      ],
      (p) => `blob:${p.partId}`,
    )
    expect(urls['logo.png']).toBe('blob:0')
  })

  test('parts with neither id nor filename contribute no keys', () => {
    expect(buildInlineImageUrls([part()], () => 'blob:one')).toEqual({})
  })
})

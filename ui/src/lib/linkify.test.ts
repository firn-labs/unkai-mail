import { describe, expect, test } from 'vitest'
import { linkify, hasLinks } from './linkify'

describe('linkify', () => {
  test('text without URLs is a single text segment', () => {
    expect(linkify('Quarterly planning agenda')).toEqual([
      { kind: 'text', text: 'Quarterly planning agenda' },
    ])
  })

  test('empty string yields no segments', () => {
    expect(linkify('')).toEqual([])
  })

  test('single URL surrounded by text', () => {
    expect(linkify('Join here: https://example.com/room before 9')).toEqual([
      { kind: 'text', text: 'Join here: ' },
      {
        kind: 'link',
        text: 'https://example.com/room',
        href: 'https://example.com/room',
      },
      { kind: 'text', text: ' before 9' },
    ])
  })

  test('URL at the very start and very end', () => {
    expect(linkify('https://example.com')).toEqual([
      { kind: 'link', text: 'https://example.com', href: 'https://example.com' },
    ])
  })

  test('plain http (not just https) is matched', () => {
    expect(linkify('see http://example.org/x')).toEqual([
      { kind: 'text', text: 'see ' },
      { kind: 'link', text: 'http://example.org/x', href: 'http://example.org/x' },
    ])
  })

  test('multiple URLs across lines keep the newlines as text', () => {
    expect(
      linkify('Agenda:\nhttps://a.example/1\nhttps://b.example/2\n'),
    ).toEqual([
      { kind: 'text', text: 'Agenda:\n' },
      { kind: 'link', text: 'https://a.example/1', href: 'https://a.example/1' },
      { kind: 'text', text: '\n' },
      { kind: 'link', text: 'https://b.example/2', href: 'https://b.example/2' },
      { kind: 'text', text: '\n' },
    ])
  })

  test('trailing sentence punctuation stays outside the link', () => {
    expect(linkify('(details: https://example.com/agenda).')).toEqual([
      { kind: 'text', text: '(details: ' },
      {
        kind: 'link',
        text: 'https://example.com/agenda',
        href: 'https://example.com/agenda',
      },
      { kind: 'text', text: ').' },
    ])
  })

  test('balanced parentheses inside the URL are kept', () => {
    expect(linkify('read https://en.example.org/wiki/Foo_(bar) first')).toEqual([
      { kind: 'text', text: 'read ' },
      {
        kind: 'link',
        text: 'https://en.example.org/wiki/Foo_(bar)',
        href: 'https://en.example.org/wiki/Foo_(bar)',
      },
      { kind: 'text', text: ' first' },
    ])
  })

  test('angle-bracket-wrapped URL (common in plain-text mail)', () => {
    expect(linkify('Meeting: <https://example.com/call/abc>')).toEqual([
      { kind: 'text', text: 'Meeting: <' },
      {
        kind: 'link',
        text: 'https://example.com/call/abc',
        href: 'https://example.com/call/abc',
      },
      { kind: 'text', text: '>' },
    ])
  })

  test('URL with query string and fragment survives untouched', () => {
    const url = 'https://example.com/join?id=42&pwd=x#room'
    expect(linkify(url)).toEqual([{ kind: 'link', text: url, href: url }])
  })

  test('bare scheme with no host is left as text', () => {
    expect(linkify('the https:// prefix means TLS')).toEqual([
      { kind: 'text', text: 'the https:// prefix means TLS' },
    ])
  })

  test('non-http schemes are not linkified', () => {
    expect(linkify('write to mailto:alex@example.com or ftp://x')).toEqual([
      { kind: 'text', text: 'write to mailto:alex@example.com or ftp://x' },
    ])
  })

  test('markup in the text stays plain text', () => {
    expect(linkify('<script>alert(1)</script>')).toEqual([
      { kind: 'text', text: '<script>alert(1)</script>' },
    ])
  })
})

describe('hasLinks', () => {
  test('true when a URL is present', () => {
    expect(hasLinks('join https://example.com')).toBe(true)
  })

  test('false for plain prose', () => {
    expect(hasLinks('no links here')).toBe(false)
  })
})

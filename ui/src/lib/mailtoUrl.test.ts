import { describe, expect, test } from 'vitest'
import { parseMailtoUrl } from './mailtoUrl'

describe('parseMailtoUrl', () => {
  test('bare recipient', () => {
    expect(parseMailtoUrl('mailto:alice@example.org')).toEqual({
      to: 'alice@example.org',
    })
  })

  test('subject + body via query', () => {
    expect(
      parseMailtoUrl('mailto:alice@example.org?subject=Hi&body=Hello%20there'),
    ).toEqual({
      to: 'alice@example.org',
      subject: 'Hi',
      body: 'Hello there',
    })
  })

  test('+ in body decodes as space (form-encoded fallback)', () => {
    expect(parseMailtoUrl('mailto:alice@example.org?body=line+one')).toEqual({
      to: 'alice@example.org',
      body: 'line one',
    })
  })

  test('cc and bcc query params', () => {
    expect(
      parseMailtoUrl(
        'mailto:a@example.org?cc=b@example.org&bcc=c@example.org',
      ),
    ).toEqual({
      to: 'a@example.org',
      cc: 'b@example.org',
      bcc: 'c@example.org',
    })
  })

  test('to= query param appends to recipients', () => {
    expect(
      parseMailtoUrl('mailto:a@example.org?to=b@example.org&to=c@example.org'),
    ).toEqual({
      to: 'a@example.org, b@example.org, c@example.org',
    })
  })

  test('no recipient, only query', () => {
    expect(parseMailtoUrl('mailto:?to=a@example.org&subject=Yo')).toEqual({
      to: 'a@example.org',
      subject: 'Yo',
    })
  })

  test('bare "mailto:" returns empty', () => {
    expect(parseMailtoUrl('mailto:')).toEqual({})
  })

  test('case-insensitive scheme prefix', () => {
    expect(parseMailtoUrl('MAILTO:alice@example.org')).toEqual({
      to: 'alice@example.org',
    })
  })

  test('case-insensitive query key', () => {
    expect(
      parseMailtoUrl('mailto:a@example.org?Subject=Hello&BODY=World'),
    ).toEqual({
      to: 'a@example.org',
      subject: 'Hello',
      body: 'World',
    })
  })

  test('malformed percent-encoding falls back to raw token', () => {
    // `%ZZ` is not a valid escape; decodeURIComponent throws.
    expect(parseMailtoUrl('mailto:a@example.org?body=bad%ZZ')).toEqual({
      to: 'a@example.org',
      body: 'bad%ZZ',
    })
  })

  test('percent-encoded recipient (e.g. + in local part)', () => {
    expect(parseMailtoUrl('mailto:alice%2Bnimbus@example.org')).toEqual({
      to: 'alice+nimbus@example.org',
    })
  })

  test('empty query value yields empty string', () => {
    expect(parseMailtoUrl('mailto:a@example.org?subject=')).toEqual({
      to: 'a@example.org',
      subject: '',
    })
  })

  test('value-less query key yields empty string', () => {
    expect(parseMailtoUrl('mailto:a@example.org?subject')).toEqual({
      to: 'a@example.org',
      subject: '',
    })
  })
})

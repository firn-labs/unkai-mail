import { describe, expect, test } from 'vitest'
import { parseFromHeader, senderLabel } from './fromHeader'

describe('parseFromHeader', () => {
  test('name + angle-bracketed email', () => {
    expect(parseFromHeader('Alice Smith <alice@example.org>')).toEqual({
      name: 'Alice Smith',
      email: 'alice@example.org',
    })
  })

  test('quoted display name (preserves embedded comma)', () => {
    expect(parseFromHeader('"Smith, Alice" <alice@example.org>')).toEqual({
      name: 'Smith, Alice',
      email: 'alice@example.org',
    })
  })

  test('bare email with no display name', () => {
    expect(parseFromHeader('alice@example.org')).toEqual({
      name: '',
      email: 'alice@example.org',
    })
  })

  test('empty / nullish input', () => {
    expect(parseFromHeader('')).toEqual({ name: '', email: '' })
    expect(parseFromHeader(null)).toEqual({ name: '', email: '' })
    expect(parseFromHeader(undefined)).toEqual({ name: '', email: '' })
  })

  test('display-name-only string (no @)', () => {
    expect(parseFromHeader('Mailer Daemon')).toEqual({
      name: 'Mailer Daemon',
      email: '',
    })
  })

  test('surrounding whitespace is trimmed', () => {
    expect(parseFromHeader('   Bob <bob@example.org>  ')).toEqual({
      name: 'Bob',
      email: 'bob@example.org',
    })
  })
})

describe('senderLabel', () => {
  test('prefers parsed name', () => {
    expect(senderLabel({ name: 'Alice', email: 'a@example.org' })).toBe('Alice')
  })

  test('falls back to local-part when name is missing', () => {
    expect(senderLabel({ name: '', email: 'alice@example.org' })).toBe('alice')
  })

  test('uses whole email when there is no @', () => {
    expect(senderLabel({ name: '', email: 'weird-token' })).toBe('weird-token')
  })

  test('empty parse → empty label', () => {
    expect(senderLabel({ name: '', email: '' })).toBe('')
  })
})

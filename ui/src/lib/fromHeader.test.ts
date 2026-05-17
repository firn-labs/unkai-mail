import { describe, expect, test } from 'vitest'
import { nameInitials, parseFromHeader, senderLabel } from './fromHeader'

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

describe('nameInitials', () => {
  test('two-word name → first + last letter', () => {
    expect(nameInitials('Max Mustermann')).toBe('MM')
  })

  test('single word → single letter', () => {
    expect(nameInitials('Max')).toBe('M')
  })

  test('three words → first + last (skip middle)', () => {
    expect(nameInitials('Max von Mustermann')).toBe('MM')
  })

  test('comma-separated last-first name', () => {
    expect(nameInitials('Smith, Alice')).toBe('SA')
  })

  test('lowercase becomes uppercase', () => {
    expect(nameInitials('alice')).toBe('A')
  })

  test('empty / nullish → ?', () => {
    expect(nameInitials('')).toBe('?')
    expect(nameInitials(null)).toBe('?')
    expect(nameInitials(undefined)).toBe('?')
  })

  test('extra whitespace is collapsed', () => {
    expect(nameInitials('   Max   Mustermann  ')).toBe('MM')
  })

  test('accented Latin letters', () => {
    expect(nameInitials('Émilie Renaud')).toBe('ÉR')
  })

  test('non-letter punctuation in a token is skipped', () => {
    expect(nameInitials('Max Mustermann (Berlin)')).toBe('MB')
  })

  test('token with no letters is skipped, next word takes its slot', () => {
    expect(nameInitials('🎉 Alice Smith')).toBe('AS')
  })
})

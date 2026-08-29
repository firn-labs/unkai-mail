import { describe, expect, it } from 'vitest'
import { profileWindowTitle } from './profileIcon'

// Node-environment tests for the pure half of `profileIcon.ts`
// (#536).  The canvas compositing path needs a real DOM and is
// exercised in the app; the Svelte/DOM imports it uses are lazy so
// this module stays importable here.

describe('profileWindowTitle', () => {
  it('keeps the plain app name for single-profile installs', () => {
    expect(
      profileWindowTitle(
        { name: 'Default', icon: { kind: 'emoji', value: '🦊' } },
        false,
      ),
    ).toBe('Unkai Mail')
  })

  it('prefixes the emoji and appends the profile name', () => {
    expect(
      profileWindowTitle({ name: 'Work', icon: { kind: 'emoji', value: '🦊' } }, true),
    ).toBe('🦊 Unkai Mail — Work')
  })

  it('trims whitespace-padded emoji values', () => {
    expect(
      profileWindowTitle({ name: 'Work', icon: { kind: 'emoji', value: ' 🦊 ' } }, true),
    ).toBe('🦊 Unkai Mail — Work')
  })

  it('drops the prefix for an empty emoji value', () => {
    expect(
      profileWindowTitle({ name: 'Work', icon: { kind: 'emoji', value: '  ' } }, true),
    ).toBe('Unkai Mail — Work')
  })

  it('uses the bare name form for named icons', () => {
    expect(
      profileWindowTitle(
        { name: 'Private', icon: { kind: 'named', value: 'contacts' } },
        true,
      ),
    ).toBe('Unkai Mail — Private')
  })
})

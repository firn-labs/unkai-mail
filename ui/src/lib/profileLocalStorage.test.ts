import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

// The module resolves the window's profile through the api layer
// at import time — stub the round-trip with a fixed id.  The
// URL-param seed is exercised separately via the windowContext
// mock's mutable export object.
const getCurrentProfileMock = vi.fn(async () => 'profile-abc')
vi.mock('./api', () => ({
  profiles: {
    getCurrentProfile: () => getCurrentProfileMock(),
  },
}))
vi.mock('./windowContext', () => ({
  windowProfileParam: null,
  parentWindowLabel: null,
}))

class MemoryStorage {
  private store = new Map<string, string>()
  getItem(key: string): string | null {
    return this.store.has(key) ? this.store.get(key)! : null
  }
  setItem(key: string, value: string): void {
    this.store.set(key, value)
  }
  removeItem(key: string): void {
    this.store.delete(key)
  }
}

let storage: MemoryStorage
beforeEach(() => {
  vi.resetModules()
  storage = new MemoryStorage()
  ;(globalThis as unknown as { localStorage: MemoryStorage }).localStorage = storage
  getCurrentProfileMock.mockClear()
  getCurrentProfileMock.mockImplementation(async () => 'profile-abc')
})
afterEach(() => {
  delete (globalThis as unknown as { localStorage?: MemoryStorage }).localStorage
})

async function importResolved() {
  const mod = await import('./profileLocalStorage')
  // Let the import-time refreshWindowProfile() promise settle.
  await new Promise((r) => setTimeout(r, 0))
  return mod
}

describe('profileScopedKey', () => {
  test('embeds the resolved profile id', async () => {
    const { profileScopedKey } = await importResolved()
    expect(profileScopedKey('trusted-senders')).toBe(
      'unkai.profile-abc.trusted-senders',
    )
  })

  test('returns null while the profile id is unresolved', async () => {
    getCurrentProfileMock.mockImplementation(
      () => new Promise(() => {}), // never resolves
    )
    const mod = await import('./profileLocalStorage')
    expect(mod.profileScopedKey('trusted-senders')).toBeNull()
  })
})

describe('adoptLegacyKey', () => {
  test('moves a pre-#535 machine-global value into the profile scope', async () => {
    storage.setItem('unkai-legacy-thing', '["a"]')
    const { adoptLegacyKey } = await importResolved()
    adoptLegacyKey('unkai-legacy-thing', 'thing')
    expect(storage.getItem('unkai.profile-abc.thing')).toBe('["a"]')
    expect(storage.getItem('unkai-legacy-thing')).toBeNull()
  })

  test('never overwrites an existing scoped value', async () => {
    storage.setItem('unkai-legacy-thing', '["old"]')
    storage.setItem('unkai.profile-abc.thing', '["new"]')
    const { adoptLegacyKey } = await importResolved()
    adoptLegacyKey('unkai-legacy-thing', 'thing')
    expect(storage.getItem('unkai.profile-abc.thing')).toBe('["new"]')
    expect(storage.getItem('unkai-legacy-thing')).toBeNull()
  })
})

describe('refreshWindowProfile', () => {
  test('re-keys after a switch-in-place', async () => {
    const mod = await importResolved()
    expect(mod.profileScopedKey('x')).toBe('unkai.profile-abc.x')
    getCurrentProfileMock.mockImplementation(async () => 'profile-def')
    await mod.refreshWindowProfile()
    expect(mod.profileScopedKey('x')).toBe('unkai.profile-def.x')
  })
})

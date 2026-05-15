import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

// The settings-bundle module pulls in Tauri runtime APIs that
// don't exist in node.  We only care that the notify hook
// fires after each mutation — stubbing the whole module keeps
// the test environment minimal (no jsdom, no Tauri shims).
const notifyMock = vi.fn(async () => {})
vi.mock('./settingsBundle', () => ({
  notifySettingsChanged: notifyMock,
}))

// In-memory localStorage stub.  vitest's default `environment:
// 'node'` doesn't ship one; we set this on `globalThis` before
// importing the module under test so the import-time module
// init sees it the same way the browser would.
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
  clear(): void {
    this.store.clear()
  }
}

let storage: MemoryStorage
beforeEach(() => {
  storage = new MemoryStorage()
  ;(globalThis as unknown as { localStorage: MemoryStorage }).localStorage = storage
  notifyMock.mockClear()
})
afterEach(() => {
  delete (globalThis as unknown as { localStorage?: MemoryStorage }).localStorage
})

// Dynamic import inside each test so module init re-runs
// against the fresh storage stub — top-level imports would
// bind to the first stub and leak state across tests.
async function importModule() {
  return await import('./trustedSenders')
}

describe('getSenderAddress', () => {
  test('extracts the angle-bracketed address from a full From header', async () => {
    const { getSenderAddress } = await importModule()
    expect(getSenderAddress('"Jane Doe" <jane@example.org>')).toBe('jane@example.org')
  })

  test('lower-cases the result so case-only variations collapse', async () => {
    const { getSenderAddress } = await importModule()
    expect(getSenderAddress('Sender@Example.COM')).toBe('sender@example.com')
  })

  test('falls back to the whole string when no brackets are present', async () => {
    const { getSenderAddress } = await importModule()
    expect(getSenderAddress(' alex@example.com ')).toBe('alex@example.com')
  })
})

describe('addTrustedSender', () => {
  test('persists the address and notifies the sync worker', async () => {
    const { addTrustedSender, isSenderTrusted } = await importModule()
    addTrustedSender('"Alex Morgan" <alex@example.com>')
    expect(isSenderTrusted('alex@example.com')).toBe(true)
    expect(notifyMock).toHaveBeenCalledTimes(1)
  })

  test('is a no-op (and skips notify) when the address is already trusted', async () => {
    const { addTrustedSender } = await importModule()
    addTrustedSender('jane@example.org')
    notifyMock.mockClear()
    addTrustedSender('"Jane Doe" <jane@example.org>')
    expect(notifyMock).not.toHaveBeenCalled()
  })

  test('treats case-only variations as the same address', async () => {
    const { addTrustedSender, listTrustedSenders } = await importModule()
    addTrustedSender('Jane@Example.Org')
    addTrustedSender('jane@example.org')
    expect(listTrustedSenders()).toEqual(['jane@example.org'])
  })
})

describe('removeTrustedSender', () => {
  test('removes a previously-added address and notifies', async () => {
    const { addTrustedSender, removeTrustedSender, isSenderTrusted } = await importModule()
    addTrustedSender('alex@example.com')
    notifyMock.mockClear()
    removeTrustedSender('alex@example.com')
    expect(isSenderTrusted('alex@example.com')).toBe(false)
    expect(notifyMock).toHaveBeenCalledTimes(1)
  })

  test('accepts a full From header and normalises before removing', async () => {
    const { addTrustedSender, removeTrustedSender, isSenderTrusted } = await importModule()
    addTrustedSender('jane@example.org')
    removeTrustedSender('"Jane Doe" <Jane@Example.org>')
    expect(isSenderTrusted('jane@example.org')).toBe(false)
  })

  test('is a no-op when the address is not present', async () => {
    const { removeTrustedSender } = await importModule()
    removeTrustedSender('ghost@example.com')
    expect(notifyMock).not.toHaveBeenCalled()
  })
})

describe('listTrustedSenders', () => {
  test('returns the addresses sorted alphabetically', async () => {
    const { addTrustedSender, listTrustedSenders } = await importModule()
    addTrustedSender('charlie@example.com')
    addTrustedSender('alex@example.com')
    addTrustedSender('Bob@example.com')
    expect(listTrustedSenders()).toEqual([
      'alex@example.com',
      'bob@example.com',
      'charlie@example.com',
    ])
  })

  test('returns an empty array when the key is unset', async () => {
    const { listTrustedSenders } = await importModule()
    expect(listTrustedSenders()).toEqual([])
  })
})

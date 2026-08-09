import { describe, it, expect } from 'vitest'
import { buildFolderRows, type TreeFolderLike } from './folderTree'

interface TestFolder extends TreeFolderLike {
  name: string
  delimiter: string | null
  standard?: boolean
}

function f(name: string, opts: { delimiter?: string | null; standard?: boolean } = {}): TestFolder {
  return { name, delimiter: opts.delimiter ?? '/', standard: opts.standard ?? false }
}

/** Leaf-name comparison mirroring the sidebar's displayName sort. */
function compare(a: TestFolder, b: TestFolder): number {
  const leaf = (x: TestFolder) => {
    const parts = x.name.split(x.delimiter ?? '/')
    return parts[parts.length - 1] || x.name
  }
  return leaf(a).localeCompare(leaf(b), undefined, { sensitivity: 'base', numeric: true })
}

function build(folders: TestFolder[], standardRoots?: TestFolder[]) {
  const roots = standardRoots ?? folders.filter((x) => x.standard)
  return buildFolderRows(folders, roots, (x) => x.standard === true, compare)
}

/** Compact `[name, depth]` view of a tier for terse assertions. */
function rows(tier: { folder: TestFolder; depth: number }[]): [string, number][] {
  return tier.map((r) => [r.folder.name, r.depth])
}

describe('buildFolderRows', () => {
  it('nests subfolders directly under their parent, indented one level', () => {
    const { custom } = build([
      f('Projects'),
      f('Receipts'),
      f('Projects/2026'),
      f('Projects/2025'),
    ])
    expect(rows(custom)).toEqual([
      ['Projects', 0],
      ['Projects/2025', 1],
      ['Projects/2026', 1],
      ['Receipts', 0],
    ])
  })

  it('handles multi-level nesting', () => {
    const { custom } = build([
      f('Projects/2026/Invoices'),
      f('Projects'),
      f('Projects/2026'),
    ])
    expect(rows(custom)).toEqual([
      ['Projects', 0],
      ['Projects/2026', 1],
      ['Projects/2026/Invoices', 2],
    ])
  })

  it('attaches subfolders of standard folders under the standard root', () => {
    const inbox = f('INBOX', { standard: true })
    const { standard, custom } = build([inbox, f('INBOX/Work'), f('Misc')])
    expect(rows(standard)).toEqual([
      ['INBOX', 0],
      ['INBOX/Work', 1],
    ])
    expect(rows(custom)).toEqual([['Misc', 0]])
  })

  it('keeps namespace-prefixed special-use folders as depth-0 roots', () => {
    // Servers that expose everything under the inbox namespace
    // ("INBOX.Sent") must not indent Sent under Inbox — special-use
    // wins over the path.
    const inbox = f('INBOX', { delimiter: '.', standard: true })
    const sent = f('INBOX.Sent', { delimiter: '.', standard: true })
    const { standard, custom } = build(
      [inbox, sent, f('INBOX.Receipts', { delimiter: '.' })],
      [inbox, sent],
    )
    expect(rows(standard)).toEqual([
      ['INBOX', 0],
      ['INBOX.Receipts', 1],
      ['INBOX.Sent', 0],
    ])
    expect(custom).toEqual([])
  })

  it('falls back to the nearest existing ancestor when intermediates are missing', () => {
    const { custom } = build([f('Projects'), f('Projects/2026/Invoices')])
    expect(rows(custom)).toEqual([
      ['Projects', 0],
      ['Projects/2026/Invoices', 1],
    ])
  })

  it('renders an orphan subfolder as a top-level custom row', () => {
    const { custom } = build([f('Ghost/Child'), f('Aardvark')])
    expect(rows(custom)).toEqual([
      ['Aardvark', 0],
      ['Ghost/Child', 0],
    ])
  })

  it('sorts siblings by leaf name at every level', () => {
    const { custom } = build([
      f('b'),
      f('a'),
      f('a/z'),
      f('a/y'),
      f('a/10'),
      f('a/9'),
    ])
    // `numeric: true` puts 9 before 10 despite code-point order.
    expect(rows(custom)).toEqual([
      ['a', 0],
      ['a/9', 1],
      ['a/10', 1],
      ['a/y', 1],
      ['a/z', 1],
      ['b', 0],
    ])
  })

  it('supports synthetic standard roots that are not in the folder list', () => {
    const outbox = f('Outbox', { delimiter: null, standard: true })
    const { standard, custom } = build([f('Misc')], [outbox])
    expect(rows(standard)).toEqual([['Outbox', 0]])
    expect(rows(custom)).toEqual([['Misc', 0]])
  })

  it("uses each folder's own delimiter when walking ancestors", () => {
    const { custom } = build([
      f('Lists.Rust', { delimiter: '.' }),
      f('Lists', { delimiter: '.' }),
    ])
    expect(rows(custom)).toEqual([
      ['Lists', 0],
      ['Lists.Rust', 1],
    ])
  })
})

// Shared attachment-open dispatch (#162).
//
// Both MailView (received message attachments) and Compose
// (chips below the editor) need the same "click → open the
// best viewer for this file type" behaviour.  This module is
// the single source of truth for that decision so the two
// surfaces stay in lockstep.
//
// Type tiers, in priority order:
//
//   - **Office** docs (.docx, .xlsx, .odt, …) — uploaded to
//     the user's Nextcloud and opened in a fresh Tauri webview
//     pointing at NC's Files app, which routes to whichever
//     handler the server has registered (Collabora for
//     `.docx`, etc.).
//   - **PDF** — same upload-to-NC path; NC's built-in PDF
//     viewer handles it.
//   - **Markdown** — same upload-to-NC path; NC routes
//     `text/markdown` to its Text app, which renders + edits
//     `.md` natively.  The temp file is DAV-deleted when the
//     viewer window closes, so any edits the user made are
//     discarded — `office_open_attachment` is the right
//     primitive because the URL it returns
//     (`index.php/f/<fileid>`) is "open with NC's default app
//     for this MIME type", which lights up Text for markdown
//     just like it lights up Collabora for `.docx`.
//   - **Everything else** — handed to the OS via Rust's
//     `print_attachment` command (despite the name, it's just
//     "drop in temp dir + ShellExecute / xdg-open").  This is
//     the new default for non-NC-renderable types.
//
// All openers take `(filename, contentType, getBytes)` so
// callers can either pass already-in-memory bytes (Compose) or
// a thunk that fetches them lazily (MailView, where bytes come
// from a Tauri IPC against IMAP).

import * as api from './api'
import { isNextcloudSource } from './ncSources'

const OFFICE_MIME_TYPES = new Set([
  'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
  'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
  'application/vnd.openxmlformats-officedocument.presentationml.presentation',
  'application/vnd.oasis.opendocument.text',
  'application/vnd.oasis.opendocument.spreadsheet',
  'application/vnd.oasis.opendocument.presentation',
  'application/msword',
  'application/vnd.ms-excel',
  'application/vnd.ms-powerpoint',
  'text/csv',
])
const OFFICE_EXTENSIONS = new Set([
  'docx', 'xlsx', 'pptx', 'odt', 'ods', 'odp', 'doc', 'xls', 'ppt', 'csv',
])

const MARKDOWN_MIME_TYPES = new Set([
  'text/markdown',
  'text/x-markdown',
])
const MARKDOWN_EXTENSIONS = new Set(['md', 'markdown', 'mkd', 'mdown'])

function fileExt(filename: string): string {
  const dot = filename.lastIndexOf('.')
  return dot < 0 ? '' : filename.slice(dot + 1).toLowerCase()
}

export interface AttachmentLike {
  filename: string
  content_type: string
}

export function isOfficeAttachment(att: AttachmentLike): boolean {
  if (OFFICE_MIME_TYPES.has(att.content_type)) return true
  return OFFICE_EXTENSIONS.has(fileExt(att.filename))
}

export function isPdfAttachment(att: AttachmentLike): boolean {
  if (att.content_type === 'application/pdf') return true
  return att.filename.toLowerCase().endsWith('.pdf')
}

export function isMarkdownAttachment(att: AttachmentLike): boolean {
  if (MARKDOWN_MIME_TYPES.has(att.content_type)) return true
  return MARKDOWN_EXTENSIONS.has(fileExt(att.filename))
}

/** Returns the user-facing label for the chip's primary action
 *  button.  Drives both MailView's chip-button text and the
 *  click-target tooltip so the two surfaces stay in sync. */
export function attachmentPrimaryActionLabel(att: AttachmentLike): {
  label: string
  iconName: 'open-in-browser' | 'open-on-desktop'
} {
  if (isOfficeAttachment(att)) return { label: 'Open in Office', iconName: 'open-in-browser' }
  if (isPdfAttachment(att)) return { label: 'Open PDF', iconName: 'open-in-browser' }
  if (isMarkdownAttachment(att)) return { label: 'Open Markdown', iconName: 'open-in-browser' }
  return { label: 'Open', iconName: 'open-on-desktop' }
}

interface OpenResult {
  url: string
  tempPath: string
}

/** Common upload-to-NC + open-in-webview machinery used by the
 *  Office and PDF (and now Markdown-via-NC if we ever put it
 *  there) flows.  Picks the first connected NC; surfaces a
 *  user-actionable error if none is connected. */
async function openViaNcViewer(
  command: 'office_open_attachment' | 'pdf_open_attachment',
  filename: string,
  contentType: string | null,
  bytes: number[],
): Promise<void> {
  // The embedded viewer uploads via Nextcloud WebDAV — generic-DAV /
  // local sources (#413) can't serve it.
  const ncAccounts = (
    await api.nextcloud.getNextcloudAccounts()
  ).filter(isNextcloudSource)
  if (ncAccounts.length === 0) {
    throw new Error(
      'Connect a Nextcloud account in Settings to open this file in the embedded viewer.',
    )
  }
  const ncId = ncAccounts[0].id
  const result: OpenResult =
    command === 'pdf_open_attachment'
      ? await api.system.pdfOpenAttachment({ ncId, filename, data: bytes, contentType })
      : await api.system.officeOpenAttachment({ ncId, filename, data: bytes, contentType })

  const { WebviewWindow } = await import('@tauri-apps/api/webviewWindow')
  const label = `att-${crypto.randomUUID().replaceAll('-', '')}`
  const win = new WebviewWindow(label, {
    url: result.url,
    title: filename,
    width: 1200,
    height: 800,
  })
  // Cleanup hook fires once when the user closes the viewer
  // window — DAV-deletes the temp file from NC so the user's
  // /Unkai Mail/temp doesn't accumulate.  Errors are
  // swallowed; the startup sweeper handles orphans.
  void win.once('tauri://destroyed', async () => {
    try {
      await (command === 'pdf_open_attachment'
        ? api.system.pdfCloseAttachment({ ncId, tempPath: result.tempPath })
        : api.system.officeCloseAttachment({ ncId, tempPath: result.tempPath }))
    } catch (e) {
      console.warn('attachment close cleanup failed:', e)
    }
  })
}


/** Drop bytes into a temp dir and hand them to the OS shell so
 *  the user's default app for that file type opens them.  No
 *  read-back: we're firing a "preview / open" UX, not editing
 *  in place. */
async function openInDesktopApp(
  filename: string,
  bytes: number[],
): Promise<void> {
  await api.system.printAttachment({ fileName: filename, bytes })
}

/**
 * Single dispatch entry point.  `getBytes` lets callers either
 * pass an already-resolved byte array (Compose, where bytes
 * live in memory) or a function that fetches them lazily
 * (MailView, where bytes need an IMAP round-trip).
 */
export async function openAttachment(
  att: AttachmentLike,
  getBytes: () => Promise<number[]>,
): Promise<void> {
  if (isOfficeAttachment(att)) {
    const bytes = await getBytes()
    await openViaNcViewer('office_open_attachment', att.filename, att.content_type || null, bytes)
    return
  }
  if (isPdfAttachment(att)) {
    const bytes = await getBytes()
    await openViaNcViewer('pdf_open_attachment', att.filename, att.content_type || null, bytes)
    return
  }
  if (isMarkdownAttachment(att)) {
    // Same NC path as Office — `index.php/f/<fileid>` routes
    // text/markdown to the Text app automatically.  Tell the
    // server the upload is `text/markdown` so the routing is
    // unambiguous even when the email's Content-Type was a
    // generic application/octet-stream.
    const bytes = await getBytes()
    await openViaNcViewer('office_open_attachment', att.filename, 'text/markdown', bytes)
    return
  }
  const bytes = await getBytes()
  await openInDesktopApp(att.filename, bytes)
}

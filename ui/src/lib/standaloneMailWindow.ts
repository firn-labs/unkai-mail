/**
 * Spawn a standalone Tauri webview window for a single mail message.
 *
 * The new window is the same Vite bundle the main app uses, but
 * `main.ts` checks `view=mail` in the URL query and mounts
 * `StandaloneMail.svelte` instead of `App.svelte`. Reusing one
 * bundle keeps the build simple and means MailView's full
 * rendering logic (HTML body, attachments, etc.) is available
 * inside the standalone reader for free.
 *
 * Each window gets a fresh UUID label so multiple mails can be
 * popped out side-by-side. Closing the window is non-destructive:
 * the standalone reader has no shared state with the parent app.
 */
export async function openMailInStandaloneWindow(
  accountId: string,
  folder: string,
  uid: number,
): Promise<void> {
  const { WebviewWindow } = await import('@tauri-apps/api/webviewWindow')
  const label = `mail-${crypto.randomUUID().replaceAll('-', '')}`
  const params = new URLSearchParams({
    view: 'mail',
    account: accountId,
    folder,
    uid: uid.toString(),
  })
  new WebviewWindow(label, {
    url: `index.html?${params.toString()}`,
    title: 'Unkai Mail',
    width: 900,
    height: 700,
    minWidth: 500,
    minHeight: 400,
  })
}

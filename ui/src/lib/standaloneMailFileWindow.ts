/**
 * Spawn a standalone Tauri webview window that displays an .eml
 * file from disk (#254 — "Open with… → Unkai" flow).
 *
 * Same bundle as the main app; `main.ts` routes `view=mailfile`
 * to `StandaloneMailFile.svelte`.  The path is passed straight
 * through the URL query — Tauri's webview does the percent-
 * encoding for us as long as we hand it the raw value via
 * `URLSearchParams`.
 */
export async function openMailFileInStandaloneWindow(
  path: string,
): Promise<void> {
  const { WebviewWindow } = await import('@tauri-apps/api/webviewWindow')
  const label = `mailfile-${crypto.randomUUID().replaceAll('-', '')}`
  const params = new URLSearchParams({
    view: 'mailfile',
    path,
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

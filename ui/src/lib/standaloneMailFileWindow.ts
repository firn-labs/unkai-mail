/**
 * Spawn a standalone Tauri webview window that displays an .eml
 * file from disk (#254 — "Open with… → Unkai" flow).
 *
 * Same bundle as the main app; `main.ts` routes `view=mailfile`
 * to `StandaloneMailFile.svelte`.  The path is passed straight
 * through the URL query — `URLSearchParams` (inside the shared
 * popout helper, #535) handles the percent-encoding.
 */

import { openPopout } from './standalonePopoutWindow'

export async function openMailFileInStandaloneWindow(
  path: string,
): Promise<void> {
  await openPopout({
    view: 'mailfile',
    params: { path },
    window: {
      title: 'Unkai Mail',
      width: 900,
      height: 700,
      minWidth: 500,
      minHeight: 400,
    },
  })
}

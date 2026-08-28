/**
 * Spawn a standalone Tauri webview window for a single mail message
 * (#104).
 *
 * The new window is the same Vite bundle the main app uses, but
 * `main.ts` checks `view=mail` in the URL query and mounts
 * `StandaloneMail.svelte` instead of `App.svelte`.  State travels
 * in the URL (no payload stash — three primitives fit fine);
 * window plumbing lives in the shared `standalonePopoutWindow.ts`
 * helper (#535).
 *
 * Each window gets a fresh UUID label so multiple mails can be
 * popped out side-by-side. Closing the window is non-destructive:
 * the standalone reader has no shared state with the parent app.
 */

import { openPopout } from './standalonePopoutWindow'

export async function openMailInStandaloneWindow(
  accountId: string,
  folder: string,
  uid: number,
): Promise<void> {
  await openPopout({
    view: 'mail',
    params: { account: accountId, folder, uid: uid.toString() },
    window: {
      title: 'Unkai Mail',
      width: 900,
      height: 700,
      minWidth: 500,
      minHeight: 400,
    },
  })
}

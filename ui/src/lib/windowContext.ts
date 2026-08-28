/**
 * Per-window launch context (#535), parsed once from the URL query
 * every window is created with:
 *
 *   - `profile=<id>` — stamped by the Rust side on `profile-*`
 *     windows.  The authoritative window→profile mapping lives in
 *     the backend's ProfileRegistry (every IPC command resolves
 *     through it); this param only lets the frontend seed its
 *     profile store before the first round-trip answers.  The
 *     static `main` window has no param and asks the backend.
 *   - `parent=<label>` — stamped by the shared popout helper: the
 *     window the popout's handoff events (compose-from-mail,
 *     event-editor-saved-from-popout, …) should target.  With
 *     several profile windows open, a broadcast handoff would fire
 *     in every shell at once (#535), so popouts emit at exactly
 *     this label.
 *
 * Pure URL parsing — no Tauri imports — so any module (api layer,
 * stores, standalone components) can read it synchronously.
 */

// `window` is absent under the node-environment vitest runs —
// modules importing this transitively (api/events, the scoped-
// storage helpers) must still load there.
const params = new URLSearchParams(
  typeof window === 'undefined' ? '' : window.location.search,
)

/** The profile id this window was created for, when the URL says. */
export const windowProfileParam: string | null = params.get('profile')

/** The window label popout→main handoff events should target;
 *  `null` in primary windows (they emit under their own label). */
export const parentWindowLabel: string | null = params.get('parent')

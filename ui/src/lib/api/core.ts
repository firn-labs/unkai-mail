/**
 * Single funnel for all backend command IPC (#473).
 *
 * Components never import `@tauri-apps/api` directly — they import the
 * typed wrappers from the sibling domain modules (`api/mail`,
 * `api/contacts`, …), each of which routes through `call()` below.
 * That buys three things:
 *
 *   1. Command names and argument keys are compile-checked: renaming a
 *      Rust command or parameter breaks the build here instead of
 *      failing silently at runtime.
 *   2. Cross-cutting concerns (error normalisation, logging, a locked-
 *      vault interceptor) have exactly one place to live.
 *   3. The transport is swappable: anything that can satisfy this
 *      module's contract (e.g. HTTP in a future self-hosted build) can
 *      replace Tauri IPC without touching a single component.
 *
 * The window-management helpers (`standalone*Window.ts`,
 * `reminderPopupWindow.ts`, `attachmentOpen.ts`) are the one sanctioned
 * exception to the no-direct-Tauri rule — opening webview windows is
 * genuinely window plumbing, not backend IPC.
 */

import { invoke } from '@tauri-apps/api/core'

export function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return invoke<T>(cmd, args)
}

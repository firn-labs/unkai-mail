/**
 * Theme application helper.
 *
 * The Rust `AppSettings` struct stores two preferences:
 *   - `theme_name` — which Skeleton theme to use (cerberus, pine, …)
 *     OR a user-imported theme id (#132 tier 2).
 *   - `theme_mode` — `"system" | "light" | "dark"`
 *
 * This module turns those into the `data-theme` and `data-mode`
 * attributes on `<html>` that Skeleton's CSS variables and the
 * Tailwind `dark:` variant (overridden in `app.css`) react to.
 *
 * `system` mode means "follow the OS". We don't hand that string to
 * the CSS — instead this module reads `prefers-color-scheme` and sets
 * `data-mode="light"` or `"dark"` itself, then keeps doing so as the
 * OS preference changes. That way the CSS only ever has to look at a
 * concrete `light` / `dark` value and never branches on "system".
 *
 * Issue #132 adds tier-2 imported themes: a CSS file the user picked
 * via the Settings → Design "Import theme…" flow.  We inject the
 * file via a runtime `<link rel="stylesheet">` whose href swaps to
 * the just-picked theme on every theme switch — Skeleton's variables
 * match against `data-theme` regardless of where they're loaded from,
 * so the rest of the app works unchanged.
 */

import { convertFileSrc } from '@tauri-apps/api/core'

export type ThemeMode = 'system' | 'light' | 'dark'

export interface ThemeOption {
  /** Skeleton theme slug for stock themes — matches the file name
      under `@skeletonlabs/skeleton/themes/<slug>.css`.  For custom
      themes (#132) this is the slug declared in the imported CSS's
      `[data-theme="…"]` selector. */
  id: string
  /** Human-readable label for the picker. */
  label: string
  /** One-line description shown next to the label so the user knows
      what they're picking without trying it first. */
  description: string
  /** True for tier-2 user-imported themes — drives the "Custom"
      tag and Remove button in the picker.  Stock themes leave this
      undefined. */
  custom?: boolean
}

/**
 * Stock Skeleton themes available in every build (#132 tier 1).
 * Adding more is two steps:
 *   1. Add an `@import` for the theme in `app.css`.
 *   2. Add an entry here.
 */
export const STOCK_THEMES: ThemeOption[] = [
  { id: 'cerberus', label: 'Cerberus', description: 'Bold default with warm accents' },
  { id: 'modern', label: 'Modern', description: 'Clean, neutral, business-like' },
  { id: 'pine', label: 'Pine', description: 'Calm forest greens' },
  { id: 'rose', label: 'Rose', description: 'Soft pink, warm and friendly' },
  { id: 'vintage', label: 'Vintage', description: 'Warm sepia, retro feel' },
  { id: 'catppuccin', label: 'Catppuccin', description: 'Pastel cosy palette' },
  { id: 'concord', label: 'Concord', description: 'Cool blues, strong contrast' },
  { id: 'crimson', label: 'Crimson', description: 'Deep reds, dramatic' },
  { id: 'fennec', label: 'Fennec', description: 'Sandy desert tones' },
  { id: 'hamlindigo', label: 'Hamlindigo', description: 'Indigo + teal high-contrast' },
  { id: 'legacy', label: 'Legacy', description: 'Subdued classic Skeleton 2 palette' },
  { id: 'mint', label: 'Mint', description: 'Fresh green / teal blend' },
  { id: 'mona', label: 'Mona', description: 'Muted plum + sage' },
  { id: 'nosh', label: 'Nosh', description: 'Earthy reds and yellows' },
  { id: 'nouveau', label: 'Nouveau', description: 'Art-nouveau greens and golds' },
  { id: 'reign', label: 'Reign', description: 'Royal purples and golds' },
  { id: 'rocket', label: 'Rocket', description: 'Saturated blues, sci-fi feel' },
  { id: 'sahara', label: 'Sahara', description: 'Warm desert oranges' },
  { id: 'seafoam', label: 'Seafoam', description: 'Soft aquas, breezy' },
  { id: 'terminus', label: 'Terminus', description: 'Stark monochrome terminal vibes' },
  { id: 'vox', label: 'Vox', description: 'Dark + electric accents' },
  { id: 'wintry', label: 'Wintry', description: 'Cool icy whites and blues' },
]

/** Live list of imported themes — refreshed by App.svelte after
 *  every `import_custom_theme` / `remove_custom_theme` IPC. */
let customThemes: ThemeOption[] = []

/** Replace the runtime catalogue of imported themes.  Caller is
 *  expected to have already pushed each theme's path through
 *  `registerCustomThemePath` so a subsequent `applyTheme` call can
 *  load the right CSS file. */
export function setCustomThemes(list: ThemeOption[]): void {
  customThemes = list.map((t) => ({ ...t, custom: true }))
}

/** All themes the picker should render — stock first, then any
 *  user-imported ones flagged with `custom: true`. */
export function listThemes(): ThemeOption[] {
  return [...STOCK_THEMES, ...customThemes]
}

/** Back-compat shim — older callers read `THEMES` directly.  Proxy
 *  resolves to the live `listThemes()` snapshot on every access so
 *  imports don't need to refactor. */
export const THEMES = new Proxy([] as ThemeOption[], {
  get(_target, prop, _receiver) {
    const live = listThemes()
    return Reflect.get(live, prop, live)
  },
})

/** Fallback if a saved `theme_name` no longer exists in the live
    list (e.g. a removed custom theme, or a build downgrade). */
export const DEFAULT_THEME_ID = 'cerberus'

const DARK_MEDIA = '(prefers-color-scheme: dark)'

/** Map from a custom theme slug → its absolute on-disk path.
 *  Maintained alongside `customThemes` so `applyTheme` can swap
 *  the runtime `<link>` href when the user picks one. */
const customThemePathById = new Map<string, string>()
export function registerCustomThemePath(id: string, path: string): void {
  customThemePathById.set(id, path)
}
export function unregisterCustomThemePath(id: string): void {
  customThemePathById.delete(id)
}

const CUSTOM_LINK_ID = 'unkai-custom-theme'

function ensureCustomThemeLoaded(themeId: string): void {
  const path = customThemePathById.get(themeId)
  let link = document.getElementById(CUSTOM_LINK_ID) as HTMLLinkElement | null
  if (!path) {
    if (link) link.parentElement?.removeChild(link)
    return
  }
  // `convertFileSrc` rewrites the absolute path into the protocol
  // the Tauri webview can fetch (`asset://` on most platforms).
  const href = convertFileSrc(path)
  if (!link) {
    link = document.createElement('link')
    link.id = CUSTOM_LINK_ID
    link.rel = 'stylesheet'
    document.head.appendChild(link)
  }
  if (link.href !== href) link.href = href
}

/**
 * Apply a theme + mode to the document. Idempotent — safe to call
 * on every settings change without diffing.
 *
 * For `system` mode this resolves the OS preference once and writes
 * a concrete `light`/`dark` value. The matchMedia listener installed
 * by `installSystemModeListener` takes care of the live updates as
 * the OS preference flips later.
 */
export function applyTheme(name: string, mode: ThemeMode): void {
  const live = listThemes()
  const themeId = live.some((t) => t.id === name) ? name : DEFAULT_THEME_ID
  ensureCustomThemeLoaded(themeId)
  document.documentElement.dataset.theme = themeId

  const concreteMode: 'light' | 'dark' =
    mode === 'system' ? (prefersDark() ? 'dark' : 'light') : mode
  document.documentElement.dataset.mode = concreteMode
}

/**
 * Subscribe to OS theme changes so `system` mode follows them live.
 * Returns an unsubscribe function. Caller owns the lifecycle — the
 * App-level `$effect` in `App.svelte` re-installs whenever the user
 * switches mode (tearing down the old listener for `light`/`dark`).
 *
 * For non-system modes this is a no-op subscription that just hands
 * back a no-op cleanup, so callers don't have to branch.
 */
export function installSystemModeListener(
  mode: ThemeMode,
  themeName: string,
): () => void {
  if (mode !== 'system' || !window.matchMedia) return () => {}

  const mql = window.matchMedia(DARK_MEDIA)
  const handler = () => applyTheme(themeName, 'system')
  mql.addEventListener('change', handler)
  return () => mql.removeEventListener('change', handler)
}

function prefersDark(): boolean {
  return !!window.matchMedia && window.matchMedia(DARK_MEDIA).matches
}

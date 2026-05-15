// `defineConfig` from `vitest/config` is a thin superset of vite's
// — it adds the `test` field's type so the tsc step in `npm run
// check` accepts our test config block.  Behaves identically to
// vite's `defineConfig` for the build / dev paths.
import { defineConfig } from 'vitest/config'
import { svelte } from '@sveltejs/vite-plugin-svelte'
import tailwindcss from '@tailwindcss/vite'
import { paraglideVitePlugin } from '@inlang/paraglide-js'

// Paraglide compiles messages from `messages/{locale}.json` into
// `src/paraglide/` on every dev tick + at build time.  See #190.
//   * `outdir` is the generated module the rest of the app imports
//     via `import * as m from './paraglide/messages'`.
//   * `strategy` decides where the active locale comes from at
//     runtime — `cookie` would default to a cookie, `url-pattern`
//     to a path prefix.  We're a desktop app with no URLs, so
//     `localStorage` (persisted) + a `baseLocale` fallback is
//     simplest; the runtime exposes `setLocale()` which the
//     Settings UI calls when the user picks German / English.
export default defineConfig({
  plugins: [
    paraglideVitePlugin({
      project: './project.inlang',
      outdir: './src/paraglide',
      strategy: ['localStorage', 'preferredLanguage', 'baseLocale'],
    }),
    svelte(),
    tailwindcss(),
  ],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    outDir: 'dist',
  },
  // Vitest config — pure-function tests only.  Stays on node
  // (no jsdom) and stubs `globalThis.localStorage` per test;
  // adding component tests later would need a DOM environment
  // opted in via `// @vitest-environment` comments.
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
})

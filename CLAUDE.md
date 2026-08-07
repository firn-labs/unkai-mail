# Unkai Mail

## Vision

A modern, native desktop mail client built in Rust that stands out through deep **Nextcloud integration** — targeting both businesses and end users. The goal is to be more appealing and capable than existing alternatives by combining standard email protocols with modern APIs and tight collaboration features.

## Key Differentiators

- **Nextcloud Talk integration** — create and join Talk rooms directly from the mail client (similar to Teams integration in Outlook)
- **Nextcloud Files integration** — attach, share, and browse files from Nextcloud directly within the client
- **Contact & Calendar sync** — full sync with Nextcloud Contacts and Calendar
- **Modern protocol support** — JMAP and direct API calls alongside traditional protocols

## Tech Stack

- **Language:** Rust (core logic, protocol handling, backend)
- **UI Framework:** Tauri 2 (native desktop app with Rust backend + system webview for UI)
- **Frontend:** Svelte 5 + TypeScript + Vite
- **UI Library:** Skeleton UI v3 (Tailwind CSS-based component library, theme: cerberus)
- **Platform targets:** Windows, macOS, Linux

## Project Structure

```
unkai-mail/
├── Cargo.toml              # Workspace root
├── crates/
│   ├── unkai-core/        # Shared types, models, error handling
│   ├── unkai-imap/        # IMAP mail retrieval
│   ├── unkai-smtp/        # SMTP mail sending
│   ├── unkai-jmap/        # JMAP modern mail access
│   ├── unkai-caldav/      # CalDAV calendar sync
│   ├── unkai-carddav/     # CardDAV contact sync
│   ├── unkai-nextcloud/   # Nextcloud API (Talk, Files, OCS)
│   ├── unkai-store/       # Local storage, caching, keychain
│   ├── unkai-discovery/   # Account autoconfiguration (SRV, autoconfig)
│   ├── unkai-crypto/      # OpenPGP + S/MIME primitives
│   ├── unkai-mcp/         # Local MCP server (#438)
│   └── unkai-commands/    # Transport-agnostic application layer (#476)
├── src-tauri/              # Tauri desktop shell (command shims + chrome)
└── ui/                     # Frontend (Svelte 5 + TypeScript + Vite)
    ├── src/
    │   ├── lib/            # Svelte components
    │   ├── app.css         # Global styles (Tailwind + Skeleton)
    │   ├── App.svelte      # Root component
    │   └── main.ts         # Entry point
    └── public/             # Static assets
```

## Protocols & Integrations

| Protocol/API | Purpose | Crate |
|---|---|---|
| IMAP | Mail retrieval | `unkai-imap` |
| SMTP | Mail sending | `unkai-smtp` |
| JMAP | Modern mail access (where supported) | `unkai-jmap` |
| CalDAV | Calendar sync (Nextcloud + others) | `unkai-caldav` |
| CardDAV | Contact sync (Nextcloud + others) | `unkai-carddav` |
| Nextcloud OCS/API | Talk rooms, file sharing, app integrations | `unkai-nextcloud` |

## Architecture Principles

- **Separation of concerns** — Rust core library handles all protocol/business logic; UI layer is a thin presentation layer
- **Offline-first** — local caching and sync so the client works without constant connectivity
- **Security-first** — TLS everywhere, credential storage via OS keychain, no plaintext secrets
- **Modular design** — each protocol as its own crate for testability and reuse

## Frontend ↔ backend IPC: the `api/` layer (#473)

All backend IPC in the frontend goes through the typed layer in [`ui/src/lib/api/`](ui/src/lib/api/) — **never import `@tauri-apps/*` directly in a component.** A vitest guard (`ui/src/lib/api/noDirectIpc.test.ts`) fails the build on violations.

- **Commands**: one typed wrapper per `#[tauri::command]`, grouped into domain modules (`api/mail`, `api/compose`, `api/accounts`, `api/contacts`, `api/calendar`, `api/nextcloud`, `api/talk`, `api/notes`, `api/tasks`, `api/crypto`, `api/settings`, `api/system`), all funnelling through `call()` in `api/core.ts`. Components do `import * as api from './api'` and call `api.mail.fetchEnvelopes({ accountId, folder, limit })`. **When you add/rename/change a Rust command, update its wrapper in the matching domain module in the same PR** — that's the point of the layer: the compiler finds every affected call site.
- **Events**: every event name (backend push channels + popout↔main handoffs) is registered in `AppEventPayloads` in `api/events.ts`. Subscribe with `api.onAppEvent('new-mail', handler)` (handler gets the Tauri `Event`, payload under `.payload`), emit with `api.emitAppEvent(...)`. Adding a new event = adding it to the registry first.
- **Platform affordances** (native dialogs, plugin notifications, autostart, `convertFileSrc` asset URLs) live in `api/platform.ts` — this file is the canonical list of desktop-only surface.
- **DTO types**: `api/types.ts` holds placeholder `any` aliases for backend DTOs. Tightening them is **lazy**, like the i18n migration: replace an alias with a real interface whenever you touch code that consumes it; don't open a bulk-typing PR.
- **Allowed exceptions** (window plumbing only, enforced by the guard test): `standalone*Window.ts`, `reminderPopupWindow.ts`, `attachmentOpen.ts` may use `@tauri-apps/api/webviewWindow`; standalone components may use `@tauri-apps/api/window` to close themselves; type-only imports from `@tauri-apps/api/event` are fine anywhere.

## Backend command layer: the `unkai-commands` crate (#476)

The backend twin of the `api/` layer above. Every `#[tauri::command]` body, the background loops, and the crypto bridge live in `crates/unkai-commands` — a crate with **no `tauri` dependency**. `src-tauri/src/main.rs` holds only thin `#[tauri::command]` shims (extract managed state → delegate) plus the desktop shell: tray, menus, windows, native notifications, deep links, URI-scheme protocols.

- **Domain modules mirror `ui/src/lib/api/` exactly** — `commands::mail` ↔ `api/mail.ts`, `commands::calendar` ↔ `api/calendar.ts`, etc. A command and its typed frontend wrapper always share a name and a domain. **When you add a Rust command: body goes in the matching `unkai-commands` module, a shim goes in `main.rs`'s alphabetical shim list + `generate_handler![]`, and the wrapper goes in the matching `api/` module — all in the same PR.**
- **`UiNotifier` (`commands::notify`) is the only channel back to the UI.** The application layer never emits Tauri events; it calls trait methods (`new_mail`, `outbox_updated`, `unread_total_changed`, …) carrying the same payload structs the frontend already deserialises. `src-tauri/src/notifier.rs` (`TauriNotifier`) is the desktop implementation — the one place that knows "notify the user" means "emit a Tauri event / repaint the tray". Adding a push channel = a trait method + a `TauriNotifier` impl + an `api/events.ts` registry entry.
- **`AppContext` (`commands::state`) bundles shared state** (`cache`, `settings`, `reminders`, `ui`) for the background loops and the few commands that notify. Plain-`Cache` commands keep taking `&Cache` — don't thread the context where it isn't needed.
- **Support modules**: `support` (helpers used by 2+ domains), `crypto_bridge` (the `CryptoBridge` impl the protocol crates call), `background` (the startup loops `main.rs` spawns), `state`, `notify`, `geocode`.
- **Keep `unkai-commands` Tauri-free.** Anything that needs an `AppHandle`, a window, or the tray belongs in `src-tauri` (either behind a `UiNotifier` method or in the shell itself). The payoff is that the whole application layer compiles and tests without a Tauri runtime.

## UI Conventions

### Glass aesthetic (#451)

The app chrome uses a frosted-glass look built on two utilities defined in `ui/src/app.css` — always reach for these instead of hand-rolling translucent backgrounds:

- **`.glass-panel`** — persistent layout chrome: IconRail, Sidebar, view headers/toolbars, integration-view nav columns, the reader action bar. Pair it with the structural border side you need (`border-r`, `border-b`); it supplies the translucent surface-derived background, blur, and border colour itself.
- **`.glass-float`** — floating layers: modal cards, context menus, dropdowns, popovers. Supplies background, blur, 1px border, and shadow — don't re-add `border`/`shadow-*` classes next to it.
- **Radii tiers**: panels/cards/modals `rounded-2xl`; compact anchored menus/dropdowns `rounded-xl`; buttons/inputs/list rows `rounded-lg`; badges/avatars/chips `rounded-full`. `rounded-md` is retired in UI chrome.
- **Hover/selected states** use translucent primary tints (`hover:bg-primary-500/10`, selected rows `bg-primary-500/12` + subtle inset ring), not solid surface fills. Transitions are 150ms ease-out.
- **Performance rules**: `backdrop-filter` lives ONLY in the two glass utilities — never per list row, and never stack two glass layers (a popover that renders *inside* a glass modal stays opaque; see EmojiPicker). All glass colours derive from Skeleton theme variables via `color-mix`, so every stock + user-imported theme keeps working; a `@supports` fallback keeps WebKitGTK (Linux) on full-value opaque surfaces.
- **Text on glass uses the contrast tokens (#453)**, never raw `text-surface-*` greys: `.text-on-glass` for primary text, `.text-on-glass-muted` for labels/placeholders (calibrated ≥4.5:1 against the composited modal chrome across stock themes; muted is the floor — don't go weaker). Skeleton `.input`/`.select` are restyled centrally in `app.css` (translucent fill, ≥3:1 border, primary focus ring) — don't re-add per-field fills or borders. Reading/writing canvases (editor content) sit on the opaque `.glass-writing-surface`, never directly on glass or near-black.
- **The white email card** (`.email-html-body` + quoted-collapse rules in `app.css`) is deliberately exempt — no glass, no radii changes there.

These are project-wide affordances we expect Claude to apply automatically when adding new list rows, sidebar items, or any other repeating element that has actions attached:

- **Left swatch is the visibility / enabled toggle.** When a row can be hidden, muted, suppressed-from-autocomplete, or otherwise disabled without removing it, expose the toggle as a small coloured square on the *left* edge of the row. Filled with the row's accent colour = enabled; outlined (transparent fill, same border colour) = disabled. The row's name text greys out (`text-surface-400 dark:text-surface-500`) when the toggle is off so the row reads as "still here, just inert" at a glance. Calendars (`CalendarView` mute swatch) and mailing lists (`ContactsView` hide-from-autocomplete swatch) are the canonical references — copy that shape rather than inventing a new disabled-state visual.
- **Three-dot button (⋯) signals "this row has actions."** Whenever a row carries any action beyond its primary click (rename, delete, change emoji/icon, hide, etc.), surface a `⋯` button on the right side of the row. Default to opacity-0 with `group-hover:opacity-100` (and persistent when its own menu is open) so resting rows stay quiet. The button must be keyboard-focusable and the menu must dismiss on outside-click and Escape.
- **Right-click does the same thing.** Every row that has a three-dot button must also respond to `oncontextmenu` by opening the *exact same* menu, anchored at the cursor position. The two surfaces share one menu component — never let them drift. This is our compatibility contract for trackpad / touchscreen users (who get the dots) versus mouse users (who reach for right-click).
- **Menu anchor pattern.** Use `position: fixed` with coordinates from `getBoundingClientRect()` (three-dot trigger) or `e.clientX/Y` (right-click). Stop `mousedown` from propagating out of the menu div — the document-level mousedown listener that dismisses it fires *before* a click, and without `stopPropagation` the menu unmounts before its item handlers run.
- **Inline edits over modals where possible.** "Rename" should swap the row's label for an `<input>` (Enter commits, Escape cancels, blur commits) — not a modal. Modals are reserved for create flows and destructive confirms.
- **Shared `EmojiPicker` for any emoji input.** Never build a one-off grid. Use `ui/src/lib/EmojiPicker.svelte` (categories + search + clear). Set `allowClear={false}` only when "no emoji" is meaningless (e.g. inserting into a text editor).
- **Outside-click dismissal idiom.** When you open a popover, register `document.addEventListener('mousedown', close)` *inside an `$effect` that depends on the open state*, with a one-tick delay (`setTimeout(..., 0)`) so the click that opened it doesn't immediately close it. Tear down on close.
- **Small action buttons are icon-only with `preset-outlined-surface-500` as the base, all the same shape.** For per-row / per-setting actions (save, edit, replace, remove, cancel, configure) use the project's standing icon-button vocabulary rather than tonal/filled buttons with text labels. The base shape is identical across neutral and destructive — only the hover overlay differs — so a Save next to a Cancel next to a Remove read as siblings, not as different button kinds.
  - **Base** (all icon buttons): `class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"` with `<Icon name="..." size={14} />` inside, plus `title=` and `aria-label=` carrying the action label. Don't add `text-xs px-2` or other spacing modifiers — the default `btn-sm` padding is the canonical width and matching everything to it is what keeps a row of icon buttons visually even.
  - **Destructive variant** (remove, delete, drop): same base + `hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40`, paired with the `trash` icon. Stays neutral at rest, turns red only on hover — so the destructive action doesn't visually shout from the page. Canonical refs: the Trusted Senders remove row in `AccountSettings.svelte`, the Remove key button in `EncryptionSettings.svelte`.
  - Canonical refs for the full set living side-by-side: the title-row Replace + Remove and the form-row Save + Cancel in `EncryptionSettings.svelte`.
- **Standard icon idioms** — reach for the registered `IconName` in [`ui/src/lib/Icon.svelte`](ui/src/lib/Icon.svelte) before introducing a new SVG:
  - `compose` → **edit / rename / replace** (project-wide "edit" glyph, including outside Compose — e.g. SecuritySettings' "Change passphrase" affordance, EncryptionSettings' "Replace key")
  - `save-draft` → **save** (inline form-confirm buttons too — see NextcloudFileBrowser's create-folder row for the canonical icon-only save / cancel pair)
  - `trash` → **remove / delete** (destructive, pair with the red-on-hover shape above)
  - `more` → **more actions / overflow / settings on a row**
  - `success` → **saved / ok / verified** (pair with a short label for status badges; see below)
  - `lock` / `encrypted` → **encryption is on**
  - `unlocked` → **password not set / no gate** (the open-padlock companion to `lock`; e.g. the share-management row chip when a share has no password)
  - `plus` → **create new** (the universal "+" glyph — New event, New room, New folder, etc.; pair with a filled-primary button to signal a primary CTA)
  - `copy` → **copy to clipboard** (two-overlapping-pages glyph; flip the icon to `success` for the post-copy "Copied" tick state)
  - `refresh` → **reload the current view** (purely client-side cache flush + re-fetch, no server-side write)
  - `sync` → **sync with the server** (semantically heavier than `refresh` — pushes / pulls state, drives a long-running task; surface with the `loading` swap below)
  - `close` → **dismiss / cancel** (X glyph — inline-form cancel buttons, modal close, etc.)
  - `share-links` → **share a public link** (chain-link glyph; the rail entry for the share manager and the row chip for "this share has a link")
  - `loading` → **action is in flight** — swap the leading icon to `loading` on stateful buttons (Refresh, Sync, Save, Sharing…, Downloading…) instead of replacing the whole label with text. Keeps the button width stable mid-action and removes a layout flicker; canonical refs are the SharesView / TalkView / FilesView header refresh buttons.
- **Status badges pair `success` with a short label, not descriptive prose.** When a toggle or setting has a saved/active confirmation, render an inline badge: `<div class="inline-flex items-center gap-1 mt-1 text-xs text-success-500" aria-live="polite"><Icon name="success" size={14} /><span>{label}</span></div>`. The badge replaces the on-state hint copy — don't render both, the badge IS the confirmation. Off-state hint copy is still useful (it explains what the toggle *would* do). Canonical ref: Encryption Settings "Passphrase saved" badge.
- **Settings panels share one button + input + dropdown vocabulary — never invent a panel-specific shape.** Every control in `AccountSettings`, `SecuritySettings`, `EncryptionSettings`, `NextcloudSettings`, and any future settings sub-panel must match an existing control in another settings panel rather than introducing a new style. Before adding a button / `<input>` / `<select>`, search the other settings files for the closest analogous control and copy its class string exactly. The Settings menu should read as one design language; a panel that uses tonal-filled buttons when its neighbours use outlined-icon, or a `border-2 border-surface-400` input when its neighbours use the default `input` shape, breaks the visual rhythm and ages the panel out of the system.
  - **Buttons**: the icon-only `preset-outlined-surface-500` shape documented above (one shape, optional red-hover overlay for destructive). No tonal/filled wide-text buttons for per-row actions.
  - **Text inputs**: `class="input flex-1 text-sm px-2 py-1 rounded-lg"` (Sidebar rename + EncryptionSettings passphrase) or the slightly larger `class="input ... text-sm px-3 py-1.5 rounded-lg"` (SecuritySettings hardware-key + passphrase enrollment). Pick the one that matches the panel you're in — don't introduce a third variant. The default Skeleton `input` border carries enough "type here" signal; don't pile on `border-2 border-surface-400` overrides.
  - **Toggle rows**: toggle on the left, then a `flex-1` column with the label + saved-badge / off-state hint + any dependent sub-form (so the form aligns with the label, not the toggle button). Canonical refs: `SecuritySettings` Key Encryption toggle, `EncryptionSettings` Unlock automatically toggle.

When in doubt, look at how `ContactsView` (mailing-list rows) and `Sidebar` (mail-folder rows) implement these — they're the canonical reference.

## Sidebar-routed integration view shell

These conventions apply to every full-pane view the `IconRail` routes to — `FilesView`, `CalendarView`, `TalkView`, `NotesView`, `ContactsView`, `SharesView`. They emerged from the #117 redesign that unified those views into one design language; future integration views should follow the same shape from day one rather than re-inventing.

- **No Close button in the header.** The `IconRail` owns navigation back to the inbox — every Close button in an integration view duplicates the rail's job. Drop the button *and* the `onclose` prop entirely; don't leave a dead prop behind. (Settings is the one exception: it's reached via the rail but uses its own shell.)
- **Three-slot header when the view has a search bar.** Title pinned left, centered `SearchInput`, action buttons pinned right. Symmetric `flex-1` slots keep the search visually centered as the translated title length changes (German vs. English):
  ```html
  <div class="flex items-center gap-3 px-6 py-3 border-b ...">
    <div class="flex-1 min-w-0 flex justify-start">
      <h2 class="text-xl font-semibold truncate">…</h2>
    </div>
    <div class="flex-1 flex justify-center min-w-0">
      <SearchInput bind:value={query} placeholder={…} class="w-full max-w-md" />
    </div>
    <div class="flex-1 flex justify-end items-center gap-2">
      … icon-only action buttons …
    </div>
  </div>
  ```
  When there's no search bar (`CalendarView` today), a plain `flex items-center justify-between` is fine — the 3-slot is only needed to keep a centered child visually centered as the side slots change width.
- **Shared `SearchInput` component**, never re-inline the magnifier-icon + clear-X pattern. [`ui/src/lib/SearchInput.svelte`](ui/src/lib/SearchInput.svelte) wraps the canonical magnifier + Skeleton `.input` + clear-X shape and is used by every "Search …" surface (mail, contacts, notes, shares, files, talk). Accepts a `children` snippet that renders inside the relative wrapper, so callers that need a popover can anchor it to the input's bounding box without re-pasting the absolute positioning. (The mail `SearchBar`'s search-syntax documentation deliberately does NOT use this — passive focus-anchored dropdowns over the mail list read as noise under the glass chrome, so it lives in a standard modal popup; see #460. The `SearchBar` itself is a slim single row whose filter glyph opens an advanced-search popout that *builds* the operator query visibly into the input — structured fields and the query string are one source of truth, and the From/To fields reuse `AddressAutocomplete` in `pickMode="replace-address"`.)
  - **The clear-X assigns `value = ''` programmatically**, which doesn't fire the underlying input's native `input` event. Callers debouncing keystrokes should react via `$effect(() => { void value; scheduleSearch() })` rather than `oninput=`, otherwise the clear button won't kick off a re-search. Canonical ref: `SearchBar.svelte`'s `$effect`-based scheduler.
- **Header action buttons are icon-only** — the same shape vocabulary as the per-row icon buttons but at the page level. Always carry both `title=` (tooltip) **and** `aria-label=` (screen reader) since the visible text label is gone; neither is optional.
  - **Primary CTA** (New event, New folder, New room, New …): `class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center"` with the action icon (`plus`, `add-folder`, …).
  - **Secondary** (Refresh, Sync): `class="btn btn-sm preset-tonal-surface inline-flex items-center justify-center"` with the matching glyph.
  - **Both swap their leading icon to `loading`** mid-action — see the `loading` entry in the icon idioms above.
- **Filter views with a `$derived` filtered list and a dedicated "no matches" empty state.** The user needs to know whether the search narrowed too far or whether the underlying data is empty — never collapse the two:
  ```svelte
  {#if rooms.length === 0}
    <p>No Talk rooms yet …</p>            {/* genuine empty */}
  {:else if filteredRooms.length === 0}
    <p>No rooms match this search.</p>     {/* filter narrowed to zero */}
  {:else}
    {#each filteredRooms as r (r.id)}…
  {/if}
  ```
- **Context-changing navigation clears the filter.** When the user moves to a different folder / calendar / list / etc., reset `searchQuery` — a filter scoped to "Documents/" is rarely also useful inside "Documents/Photos/", and matching local-explorer conventions is more predictable than carrying stale filters. Canonical ref: `FilesView` resets on `currentPath` change via a `$effect`.
- **Labeled action buttons in footers** (e.g. FilesView's "Link" / "Attachment" selection-commit row) use the *same* `btn-sm` + `inline-flex items-center gap-1.5` shape as the icon-only header buttons — just with the label after the icon. Filled-primary for the default action, outlined-primary for the alternative. Loading state swaps the leading icon to `loading` (not the label) so the button width stays stable mid-action.
- **Inline-edit form Confirm / Cancel buttons** use the icon-only shape too — `save-draft` glyph for Confirm, `close` (X) for Cancel. Canonical ref: NextcloudFileBrowser's create-folder name-input row.
- **Modal dialog shape** is consistent across views: `class="fixed inset-0 flex items-center justify-center bg-black/50"` backdrop + `class="glass-float rounded-2xl w-[28rem] max-w-full p-5"` card. Escape + outside-click both dismiss. When editing a value that's intentionally hidden (e.g. a stored password's hash), use a tri-state radio (Keep current / Remove / Replace) rather than a single text input — explicit modes prevent the "submitting blank silently clears the value" trap. Canonical ref: SharesView's edit-share modal.
- **Children-of-`NextcloudFileBrowser`-style imperative actions**: when an inner reusable browser owns state (folder navigation, listing, create-folder) and the parent wants to surface a header-level trigger for one of those actions, expose the function via the `on…ref?: (fn) => void` callback pattern Compose uses (see `oncancelref` / `onaddattachmentsref`). The child fires it once on mount with its internal function; the parent stores it in `$state` and invokes it imperatively. Pair with an optional `hide…Trigger` boolean prop so the child can drop its own inline affordance when the parent provides a header one. Canonical ref: `NextcloudFileBrowser`'s `onstartcreatefolderref` / `onrefreshref` / `hideInlineNewFolderTrigger`.
- **Localised dates via `toLocaleDateString`**, never the raw `YYYY-MM-DD` string from the server. Parse the parts and construct via the local-zone `new Date(y, m - 1, d)` ctor — the `new Date("YYYY-MM-DD")` string-arg form interprets the value as UTC midnight and silently shifts one day earlier in negative-offset zones. Canonical ref: `SharesView`'s `formatExpiryDate`.

When in doubt, copy the shape from `SharesView` — it was the first view written from scratch under these conventions and is the cleanest reference.

## Email-rendering conventions

The Talk + meeting invite cards we drop into outgoing mail (`ui/src/lib/inviteHtml.ts`, used by Compose for the "Insert Talk link" and "Respond with meeting" flows) have a few non-obvious rules that go beyond normal HTML:

- **Inline styles only.** Gmail, Outlook, Yahoo, etc. all strip `<style>` blocks from received mail; class names carry no meaning across clients. Every visual property has to live on the element via `style="..."`. No external CSS, no `@import`, no `@font-face`.
- **System font stack.** `-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif`. Crisp on every OS without a font fetch.
- **Detail-row glyphs are emoji** (📅 🕐 📍 📝 💬 🔗). Universal client support. SVG / icon fonts inside email are unreliable across Outlook desktop and conservative Gmail setups.
- **No images in the chrome.** The brand header is a typography-only wordmark in a soft pill — no `<img>`. We tried both:
  - **Remote URL** (`raw.githubusercontent.com/...`): hit "block remote images by default" in Gmail / Apple Mail / Outlook and the recipient saw a broken-icon until they trusted the sender. (Also: the original path I picked pointed at the v2 set, but storm is a v1 style — easy mistake to repeat. v2 ships `copper / forest / midnight / ocean / rose / slate / sunset`; storm lives at `logos/unkai-logo/png/storm/...`.)
  - **Inline `data:image/png;base64,…` URI**: many corporate / hardened mail filters (Outlook in particular) strip `<img src="data:…">` for security, again leaving a broken-icon.
  Both paths ate the logo. Don't reintroduce an `<img>` in the chrome unless you've solved this for the worst client your users will receive mail in. The `PUBLIC_UNKAI_LOGO_URL` export is now an empty-string compatibility stub for any leftover importers.
- **The editor's `UnkaiBlock` extension** (`ui/src/lib/RichTextEditor.svelte`) recognises `<div data-unkai-block="…">` wrappers as atom nodes so the styled cards survive Tiptap's schema. If you add a new card kind, stamp the wrapper with that data attribute and the editor will render it via the existing NodeView path — no new extension needed.

When in doubt, render the card to a local HTML file and open it in `outlook.com`, `mail.google.com`, and Apple Mail — those three are the dominant surfaces and have the strictest sanitisers.

### Inline body images — `cid:` (#471)

Incoming mail puts images *in* the body by attaching them with a Content-ID and pointing at them with an RFC 2392 URL (`<img src="cid:logo@example">`). The webview can't resolve that scheme, so the reader resolves it itself:

- **Backend** — `fetch_inline_images` (and `parse_eml_file_inline_images` for the `.eml` popout) pull **every** referenceable image part out of **one** raw-message fetch, base64-encoded. Never resolve cids by calling `download_email_attachment` in a loop: that command opens an IMAP connection and re-FETCHes the whole message *per part*. Extraction lives in `crates/unkai-imap/src/inline_images.rs`, with per-image (8 MB) and per-message (32 MB) ceilings; over-ceiling parts stay reachable as normal attachment chips.
- **Part ids are one index space.** `InlineImage.part_id` uses the same `attachments()` enumeration that stamps `EmailAttachment.part_id`, so a cid image and its attachment chip address the same bytes. Anything that re-indexes parts has to keep both in step.
- **Frontend** — `ui/src/lib/inlineImages.ts` owns the matching rules (normalise the cid, match by Content-ID then by filename) and the DOM rewrite; both readers (`MailView`, `StandaloneMailFile`) call it, and the object URLs it builds are revoked when the message closes. Match logic goes in that module, not in a component, so it stays unit-tested.
- **No "Show images" gate.** Inline parts ride inside the message, so rendering them fetches nothing from the network and leaks no read receipt. The remote-image blocker deliberately skips `cid:` — only `http(s)` sources need consent.

### Mail-body text contrast on the theme background — (#472)

The reader's "use the app's theme" mode (the non-white-canvas option, `mail_html_white_background = false`) renders the body straight on the theme canvas — near-black in dark mode. Senders write inline colours assuming a white page, so `ui/src/lib/emailContrast.ts` re-tints the ones that would be unreadable:

- **Only failing colours change.** The pass walks the sanitised body tracking the *effective* (background, inherited-text) pair — the app canvas, or any background the email brings along (`background-color` inline style or legacy `bgcolor=`). A colour below 4.5:1 WCAG contrast against what's actually behind it gets its lightness walked toward the readable pole, hue and saturation kept (dark navy link → light blue, not white). Colours that already clear the floor, and all backgrounds, stay exactly as the sender wrote them. A white table cell keeps its dark text; a white cell *relying* on client-default black text gets an explicit dark colour stamped so the theme's near-white inherited text doesn't vanish on it.
- **Colour math is pure and unit-tested** (`emailContrast.test.ts`, node-only — no DOM); only `ensureReadableEmailText` touches elements. It runs as the last pass in MailView's `processEmailHtml`, and never in white-canvas mode.
- **Theme colours are oklch**, so `getComputedStyle` strings can't be hand-parsed — MailView resolves the canvas + base text colour (and any exotic email colour syntax) by painting onto a 1×1 canvas probe and reading the pixel back.
- **Live theme flips re-tint**: a MutationObserver on `<html>`'s `data-theme`/`data-mode` bumps a version counter the `processedHtml` derived reads, so switching light/dark with a message open re-runs the pass against the new canvas.

## Development Guidelines

- Write clear, well-documented code — the team is learning as they build
- Prefer existing, well-maintained Rust crates over reimplementing protocols from scratch
- Write tests for protocol handling and data sync logic
- Use `clippy` and `rustfmt` on all Rust code
- Commit messages should explain *why*, not just *what*
- Keep the UI responsive — heavy work belongs in async background tasks, never on the UI thread
- **No other-mail-client references in code, comments, or commit messages.** Do not name Outlook, Apple Mail, Thunderbird, Gmail (as a UX comparison — the literal `gmail.com` hostname is fine in autoconfig / discovery code), Yahoo Mail, Fastmail, Spark, Airmail, Hey, ProtonMail, Tutanota, etc. Describe the *behaviour* generically ("the standard mail-client triage gesture", "the operator-prefixed search syntax") instead of comparing to a specific product. Where a comment is anchoring on a real protocol or RFC quirk, name the protocol or the RFC, not the client whose implementation first surfaced it. Applies retroactively — if you spot a leftover reference, rewrite it. Hostnames inside string literals (`gmail.com`, `[gmail]/trash`, `autoconfig.thunderbird.net`) are factual data and stay; the rule is about comments and prose.
- **No team-member names anywhere user-visible (and avoid them in code too).** Do not use `Nick`, `Jannik`, `nick@…`, `jannik@…` or any other real team-member name in: input placeholders, sample data shown in the UI, default signatures / display names, error messages, log lines, doc comments that look like example output, or test fixtures (use generic placeholders like `Alex Morgan`, `you@example.com`, `Jane Smith` instead). The rule applies to anything an end user, screenshot, or future contributor could read. The *only* places where the real names belong are project-context documents the team explicitly maintains: `CLAUDE.md` (the team-context section), `README.md` (the maintainers note), `SBOM.md` / `License.md` headers, commit / PR authorship metadata, and the GitHub repo itself. Applies retroactively — if you spot a leftover personal-name placeholder, rewrite it.
- **Localise every user-visible string via `paraglide-js` (#190).** The frontend uses [paraglide-js](https://inlang.com/m/gerre34r/library-inlang-paraglideJs) for i18n. Locales live in `ui/messages/<code>.json` (currently `en.json` + `de.json`); the compiled `ui/src/paraglide/` module is regenerated on every build by the Vite plugin. Three rules:
  - Inside Svelte components, `import { m } from '../paraglide/messages'` and call `m.<key>()` (or `m.<key>({ var: value })` for interpolations) instead of inlining English strings. Doc comments and `console.warn` lines stay English — those aren't user-facing.
  - When you add or rename a key in `en.json`, also add the German translation in `de.json`. Missing keys fall back to English at runtime, but paraglide warns at compile time, and a half-translated UI is jarring on its own.
  - Migration is **lazy**, not a forced sweep: any PR that *already* edits a UI string moves those strings into the catalogue as part of its work. New code writes against `m.foo()` from day one. Don't open a "translate everything" PR — the codebase converges over time.

  Backend Rust strings (error variants, log lines, validation copy) stay English — wrap them on the frontend with the appropriate `m.<key>(...)` when surfacing to the user. Adding a new locale is a `messages/<code>.json` + a one-line entry in `LOCALE_LABELS` inside `AccountSettings.svelte` plus an entry in `ui/project.inlang/settings.json#locales`.
- **Maintain `SBOM.md` AND `License.md` on every dependency change.** Adding, removing, or upgrading a package in any `Cargo.toml` (workspace or per-crate) or in `ui/package.json` requires edits to both:
  - `SBOM.md` — list the package, its licence, what category that licence falls into (permissive / weak copyleft / strong copyleft), and update the "Last manual reconciliation" date at the bottom of the inventory section.
  - `License.md` — add the package to the section matching its licence, or create a new section if that licence isn't represented yet (and add the licence's notice text inline if so).

  `SBOM.md` is the marketing-implications document (what each licence forces our distribution model to look like); `License.md` is the legal attribution document we ship next to binaries to satisfy each upstream's notice obligations. Introducing a stronger copyleft licence than what's already in the tree (e.g. AGPL-3.0 when we currently top out at GPL-3.0) is a project-level decision — surface it explicitly to Nick / Jannik before merging, don't slip it into a routine PR.

## Build & Run

### Windows build prerequisite: Strawberry Perl

The local mail cache is encrypted at rest via **SQLCipher**, which is
built through `rusqlite`'s `bundled-sqlcipher-vendored-openssl` feature.
That feature compiles OpenSSL from source as part of `cargo build`, and
OpenSSL's build scripts require a full Perl install.

The Perl that ships with Git Bash is a stripped-down MSYS2 Perl that
fails to find its own standard modules (`Locale::Maketext::Simple`),
so we need **Strawberry Perl**:

```powershell
# One-time install (powershell)
winget install StrawberryPerl.StrawberryPerl
```

Then make sure Strawberry Perl is found *before* Git's Perl. In Git Bash:

```bash
export PATH="/c/Strawberry/perl/bin:/c/Strawberry/c/bin:$PATH"
```

Add that to your `~/.bashrc` so every shell picks it up automatically.

**End users do not need Perl or OpenSSL installed.** The `vendored-openssl`
feature statically links the compiled OpenSSL into the final `.exe`, so
the shipped binary is self-contained. Perl is a build-time tool only.

### CI

We run a **two-tier CI model** so daily dev stays fast and the heavy security suite only kicks in around release time:

| Workflow | When it runs | What it does |
|---|---|---|
| `smoke.yml` | Every PR + push to `main` | `cargo fmt --check`, `cargo check --workspace`, frontend typecheck + build. ~2 min. |
| `ci.yml` (a.k.a. *Quality gate*) | `workflow_call` only — invoked from `release.yml` and `weekly-security.yml` | clippy, tests, cargo-audit, cargo-deny, frontend lockfile-lint + npm audit |
| `codeql.yml` / `osv-scanner.yml` / `semgrep.yml` | `workflow_call` + their own crons | Static / supply-chain scanners that publish to the Security tab |
| `release.yml` | Push of a `v*` tag (or manual dispatch for a dry-run) | Runs the full quality gate + scanners; if all green, matrix-builds Tauri installers and uploads them to a draft GitHub Release |
| `weekly-security.yml` | Sunday 06:00 UTC | Runs the full quality gate + scanners on `main` so the Security tab keeps fresh data between releases |

**What this means for daily work:**

- Push freely on any branch — only the 2-minute smoke runs.
- A red smoke means you broke `cargo check` or formatting; everything else is deferred.
- Heavy regressions surface at release time (or in the Sunday cron). For a 2-person scaffolding-phase project that's an acceptable trade-off; tighten back up once we have real users.

**How to cut a release — full recipe.** When the user says "push a new version" / "release vX.Y.Z" / "ship a new build", walk them through every step below. Do not improvise an abbreviated version of this.

> ⚠️ **The dry-run proves the gate, not the build.** The `workflow_dispatch` dry-run (and the daily smoke) run clippy / tests / scanners but **never run `tauri build` bundling** — the build matrix is gated on `refs/tags/v*`, so it only executes on a real tag. That means any *bundling* problem (missing icons, a disabled bundler, an installer-target quirk) is structurally invisible until the first tag of a new bundle config, and surfaces ~20 min into the release run. The v0.1.0 first release flushed out three of these in sequence; if you ever touch `src-tauri/tauri.conf.json`'s `bundle` block, expect the first tag to be the real test. Known requirements that bit us, all in the `bundle` block:
> - `"active": true` — Tauri 2 defaults `bundle.active` to **false**; without it `tauri build` emits only the bare binary and tauri-action fails with "No artifacts were found."
> - `"icon": [...]` — the Windows (`.ico`) and AppImage (square PNG) bundlers **hard-require** an icon; `.deb`/`.rpm`/macOS are lenient and will silently build without one, masking the gap. Point it at the existing `src-tauri/icons/` set.
> - `"targets": "all"` — emits every installer per platform (Windows `.exe`+`.msi`, macOS `.dmg`, Linux `.deb`+`.rpm`+`.AppImage`).
>
> If a tag's build fails, fix on `main` and re-tag the *same* version (the deleted tag shipped nothing) — see "If the tag pipeline fails" below.

1. **Pick the new version** — semver:
   - **patch** (`0.1.0 → 0.1.1`): bug fixes, security bumps, internal refactors. No new user-visible behaviour.
   - **minor** (`0.1.0 → 0.2.0`): new features, additive UI, new settings. Backwards-compatible.
   - **major** (`0.1.0 → 1.0.0`): breaking config-file changes, removed features, anything that requires a manual upgrade step. Pre-1.0 we're loose with this; once we ship to real users, treat it strictly.

2. **Bump the version in BOTH files** (they must stay in sync — the installer filename is built from `tauri.conf.json` while crate metadata reads from `Cargo.toml`):
   - `Cargo.toml` → `[workspace.package].version`
   - `src-tauri/tauri.conf.json` → top-level `"version"`
   - (`ui/package.json` has its own version — leave it; it's not user-visible.)

3. **Preview the auto-generated changelog** (optional but recommended — saves an "oh no, that PR didn't get a label" moment after the tag is out):

   ```bash
   gh api repos/firn-labs/unkai-mail/releases/generate-notes \
     -f tag_name=vX.Y.Z \
     -f previous_tag_name=vPREV.PREV.PREV \
     --jq .body
   ```

   If a PR ended up under "🔧 Other changes" that should have been a feature or fix, label it correctly on GitHub *before* tagging — the auto-generator runs at tag time and bakes the result.

4. **Commit the bump and push to main:**

   ```bash
   git checkout main
   git pull origin main
   # edit Cargo.toml and src-tauri/tauri.conf.json
   git add Cargo.toml Cargo.lock src-tauri/tauri.conf.json
   git commit -m "Bump version to vX.Y.Z"
   git push origin main
   ```

   Wait for `smoke.yml` to go green on main. If it fails, fix and re-push the bump commit before tagging — never tag a red main.

5. **Tag and push:**

   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

   This kicks off `release.yml`. Phase 1 runs the full quality gate (clippy / tests / audit / deny / codeql / osv / semgrep). Phase 2 only starts if every gate passed; it matrix-builds the 3-OS installers via `tauri-apps/tauri-action` and creates a *draft* GitHub Release.

6. **Watch the run** — on a cold cache it takes ~25 min. Surface any failures to the user immediately rather than leaving them to discover it on their own.

7. **Finalise the Release**:
   - Open the draft on the Releases page.
   - Paste the editorial sections from `RELEASE_NOTES_TEMPLATE.md` *above* the auto-generated changelog (the template comment block explains what goes where).
   - Verify every expected installer is attached. The `productName` is `Unkai-Mail`, so assets are named `Unkai-Mail_*` (note: the bundler builds the names from `productName`, not the crate name). The full set from a green build is:
     - Windows: `Unkai-Mail_X.Y.Z_x64-setup.exe` (NSIS) + `Unkai-Mail_X.Y.Z_x64_en-US.msi` (WiX)
     - macOS (Apple Silicon only — `macos-latest` runners are arm64): `Unkai-Mail_X.Y.Z_aarch64.dmg` + `Unkai-Mail_aarch64.app.tar.gz`. There is **no Intel (x64) `.dmg`** unless we add an x64 macOS matrix entry.
     - Linux: `Unkai-Mail_X.Y.Z_amd64.deb` (Debian/Ubuntu) + `Unkai-Mail-X.Y.Z-1.x86_64.rpm` (Fedora/RHEL) + `Unkai-Mail_X.Y.Z_amd64.AppImage` (any distro).
     - Note: tauri-action does **not** emit a `SHA256SUMS` file by default — don't promise one in the notes unless we add a hashing step.
   - Click **Publish release**.

**If the tag pipeline fails** (gate red, build matrix red, etc.):

```bash
# Delete the bad tag locally and on the remote
git tag -d vX.Y.Z
git push origin --delete vX.Y.Z
```

Then fix the underlying problem on `main` (a new commit, not an amend), and re-tag the same `vX.Y.Z` — it's fine to reuse the version number because the failed tag was deleted and no Release was published from it. The draft Release that may have been created can be left alone or deleted from the Releases UI.

**What NOT to do:**

- Don't bump only one of the two version files — you'll get installers named after the old version while crate metadata reports the new one.
- Don't tag from a branch other than `main`. The release workflow only treats `main` as canonical.
- Don't tag without pushing the bump commit first. The build will then ship the *previous* version's code under the new tag's name.
- Don't republish a tag that already shipped — bump the version instead. Mutating a published release breaks anyone who already downloaded it.

**Release notes:**

- Auto-generated changelog comes from PR titles, grouped by label. The grouping config lives in `.github/release.yml`. To route a PR into a category, label it (`feature` / `bug` / `security` / `performance` / `documentation` / `refactor` / `test` / `dependencies`).
- The hand-written editorial header lives in `RELEASE_NOTES_TEMPLATE.md` — paste that above the auto-changelog when finalising the release.
- Dependabot PRs are auto-routed to a "Dependency updates" bucket and excluded from the headline categories so the changelog stays human-curated.

**Windows builds need Strawberry Perl** (the SQLCipher → OpenSSL → vendored-openssl chain compiles OpenSSL from source, and OpenSSL's build scripts need a real Perl). Both `smoke.yml` and `release.yml` install it via `choco install strawberryperl -y` on the Windows runner. Local Windows dev needs the same — see the section above.

### Commands

```bash
# Install frontend dependencies
cd ui && npm install

# Run in development mode (starts both Vite dev server and Tauri)
cargo tauri dev

# Build for production — produces a self-contained installer/exe
cargo tauri build

# Run Rust tests
cargo test --workspace

# Lint Rust code
cargo clippy --workspace
```

## Project Status

**Phase: Scaffolding complete**
- Rust workspace with modular crates set up
- Tauri 2 + Svelte 5 + Skeleton UI frontend in place
- Basic mail client UI shell (sidebar, mail list, reading pane)
- Repository: https://github.com/firn-labs/unkai-mail
- Next: implement first protocol (IMAP), connect backend to frontend via Tauri commands

## Development Workflow

The team follows a simple loop for every issue:

1. **Pick an issue** — choose an open GitHub issue to work on
2. **Ask Claude** — use Claude Code to implement, explain, or debug. Claude uses this `CLAUDE.md` as project context, so keep it up to date
3. **Understand & revise** — review Claude's output, make sure you understand the code, adjust as needed
4. **Push to GitHub** — commit and push when the work is solid

This means Claude should:
- Always explain *what* the code does and *why* it's written that way
- Not just produce code — teach the team as you go
- Keep `CLAUDE.md` updated when the project evolves (new decisions, status changes, tech stack updates)

## Git Branching Strategy

We use **short-lived feature branches, one per issue.** This keeps PRs focused, reviews small, and avoids the long-running merge conflicts that come with permanent personal branches.

```
main (stable, always compiles)
 ├── feature/10-contacts-view       (short-lived, deleted after merge)
 ├── feature/14-settings-panel      (short-lived, deleted after merge)
 └── feature/17-imap-idle           (short-lived, deleted after merge)
```

### Rules
- **Never push directly to `main`** — always merge via Pull Request
- **One branch per issue** — name it `feature/<issue-number>-<short-slug>` (e.g. `feature/10-contacts-view`)
- **Branch from the latest `main`** — always start a new feature branch from an up-to-date `main`:
  ```bash
  git checkout main
  git pull origin main
  git checkout -b feature/<issue-number>-<short-slug>
  ```
- **When your issue is done** — open a PR from your feature branch to `main`, the other person reviews and merges
- **After the PR is merged** — delete the branch (locally and on GitHub), then start the next issue with a fresh branch off the new `main`:
  ```bash
  git checkout main
  git pull origin main
  git branch -d feature/<old-branch>
  git push origin --delete feature/<old-branch>
  ```
- **Merge early, merge small** — if you add a shared type to `unkai-core` that the other person needs, split it into its own tiny PR first so the other feature branch can use it

### When to merge to main
- The issue is complete (or a clean slice of it is)
- A new model or type is added to `unkai-core` that other work depends on
- A crate compiles and has basic functionality or tests
- A UI component works (even with mock data)
- **Do NOT merge** broken code or half-finished functions

### Claude reminder obligations
Claude MUST proactively remind the developer in these situations:

**Before opening a PR:**
> "Ready to open a PR? Double-check: you're on a feature branch named `feature/<issue-number>-<slug>`, branched from an up-to-date `main`, and this branch covers exactly one issue. If you're on `main` or a long-lived personal branch, stop and move the commits onto a proper feature branch first."

**After an issue is merged to `main`:**
> "This is now merged to main. Delete the feature branch (`git branch -d feature/<name>` locally, `git push origin --delete feature/<name>` on GitHub), then remind the other developer (Nick/Jannik) to pull main before starting their next branch: `git pull origin main`."

Together these keep both developers starting every issue from the same clean base and avoid painful merge conflicts.

## Team Context

- **Nick** and **Jannik** — two-person team, new to building a project of this scale
- AI assistance (Claude) is a core part of the development workflow for code generation, explanation, and architectural guidance
- Expect frequent questions about Rust idioms, protocol details, and design patterns — answer thoroughly with explanations
- Project management via GitHub Issues and milestones (Phases 1–3)

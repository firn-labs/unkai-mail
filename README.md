<div align="center">

<img src="https://raw.githubusercontent.com/Videothek/unkai-mail/main/logos/unkai-logo/png/storm/unkai-256.png" alt="Unkai Mail" width="160" />

# Unkai Mail

**A modern, native desktop mail client built on deep Nextcloud integration.**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/Built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![Built with Tauri 2](https://img.shields.io/badge/Built%20with-Tauri%202-24c8db.svg)](https://tauri.app)
[![Built with Svelte 5](https://img.shields.io/badge/Built%20with-Svelte%205-ff3e00.svg)](https://svelte.dev)

</div>

> ⚠️ **Project status — early development.** Mail (IMAP / SMTP) works
> end-to-end with an encrypted local cache and Nextcloud authentication
> is in place, but Unkai is not yet feature-complete or suitable as a
> daily driver. See the [Roadmap](#roadmap) and the
> [issue tracker](https://github.com/Videothek/unkai-mail/issues).

---

## What it is

Unkai is a desktop mail client for people who already live in Nextcloud.
It speaks IMAP, SMTP, and JMAP for the mail itself, but pulls Talk rooms,
Files, Contacts, and Calendar straight into the inbox so the rest of your
collaboration stack isn't one tab away. Native, fast, encrypted at rest,
and designed to feel like one app rather than five.

<!--
  SCREENSHOT: hero
  Three-pane main window in dark mode. Left rail with the active-account
  avatar + folder tree, middle column with the message list (a few
  unread rows visible), right pane with an open message showing the
  styled HTML body, an attachment chip strip, and the action toolbar.
  Roughly 1600×1000, PNG, hosted in `docs/screenshots/`.
-->
<p align="center">
  <em>📸 Screenshot placeholder — main three-pane inbox view</em>
</p>

---

## Features

### 📬 Mail that gets out of your way

Real protocols, real native rendering. The compose window is a full
rich-text editor (lists, tables, images, signatures, @-mentions). The
reading pane sandboxes HTML mail through DOMPurify, blocks remote images
by default, and renders attachments inline where it makes sense.

<!--
  SCREENSHOT: compose
  Compose window with rich-text editor open, a styled meeting invite
  card visible in the body (the one inviteHtml.ts produces), the To/Cc
  fields populated, and the toolbar showing the Insert tab. Roughly
  1200×900.
-->
<p align="center">
  <em>📸 Screenshot placeholder — compose window with rich-text + meeting invite card</em>
</p>

### ☁️ Nextcloud, all the way down

- **Talk** — create a meeting room from a thread, attach the join link
  to the outbound mail, defer participant invites until you actually
  hit Send.
- **Files** — attach files straight from your Nextcloud, share via
  password-protected public links, save inbound attachments back into
  any Nextcloud folder.
- **Contacts** (CardDAV) — full sync with Nextcloud Contacts and any
  other CardDAV server. @-mentions in compose autocomplete from your
  addressbook; mailing lists unify Contact Groups, manual `KIND:group`
  vCards, and Nextcloud Teams / Circles.
- **Calendar** (CalDAV) — RSVP to meeting invites inline. The "Respond
  with meeting" action drops a styled invite card into your reply with
  the time, location, notes, and an optional Talk room.
- **Notes** — full sync with the Nextcloud Notes app. Real markdown
  end-to-end (no HTML serializer in the middle) with a side-by-side
  preview pane. Two cross-feature autocomplete triggers live in the
  editor:
  - Type **`@`** followed by a name — a popup opens with matching
    contacts from your address book. Pick one and a clickable
    contact reference (`[Name](mailto:address)`) is inserted at the
    cursor.
  - Type **`/mail`** *then a space* — a popup opens that searches
    your cached mail (cross-folder) as you type, with a server-side
    fallback for the active account's inbox. Pick a message and a
    clickable mail reference (`[Subject](mail://account/folder/uid)`)
    is inserted.
  Mail references opened from a note pop out into a standalone
  reader window by default so the editor stays put; flip **"Open
  mails in mail view"** in Settings → General if you'd rather the
  main view jump to the message instead.

<!--
  GIF: nextcloud-talk
  Animated capture of the "Respond with meeting" flow: open a thread →
  click Respond with meeting → fill the EventEditor (auto-create Talk
  room toggled) → save → Compose opens pre-filled with the invite card
  pasted into the body. ~10 s, 800×600 webp/gif.
-->
<p align="center">
  <em>🎬 Animated demo placeholder — "Respond with meeting" + Talk room creation</em>
</p>

### 🔒 Security-first by default

- TLS everywhere, with a per-account "trust this self-signed cert"
  flow that captures the full chain so renewals stay invisible.
- All passwords (mail, Nextcloud) live in the OS keychain
  (Credential Manager / macOS Keychain / Secret Service) — never
  on disk.
- The local mail cache is encrypted at rest with **SQLCipher** (AES-256).
  The master key lives in the same OS keychain, optionally protected
  by FIDO2 PRF for hardware-backed unlock.

### 🎨 Themable, accessible, fast

- 22 stock themes plus custom CSS imports via the
  [Skeleton](https://www.skeleton.dev) Theme Generator.
- Light / Dark / Follow-OS toggle on top of any theme.
- Native performance. Tauri shell wrapping a Rust core — not a packaged
  Electron app.

<!--
  SCREENSHOT: theming
  Settings → Design panel with the theme picker grid visible (showing
  several stock themes including Cerberus and a custom imported one
  with the small "custom" tag), and the App-icon picker below it.
-->
<p align="center">
  <em>📸 Screenshot placeholder — theme + app-icon picker</em>
</p>

### 🔍 Search that scales

Local FTS5 index over the encrypted mail cache for instant searches with
operator-prefixed syntax (`from:alice subject:invoice has:attachment`).
"Search server too" falls back to IMAP `UID SEARCH` for archives that
haven't been opened on this machine yet, with infinite scroll over the
results.

<!--
  SCREENSHOT: search
  Search bar with a query like `from:alice subject:invoice` and the
  results panel showing several hits with `<mark>`-highlighted
  snippets. Capture the empty-state too (the prominent "Search server
  too" CTA).
-->
<p align="center">
  <em>📸 Screenshot placeholder — search results with operator syntax</em>
</p>

---

## Tech stack

| Layer | Choice |
|---|---|
| Core logic & protocols | Rust (workspace of focused crates) |
| Desktop shell | [Tauri 2](https://tauri.app) — Rust backend + native webview |
| Frontend | Svelte 5 + TypeScript + Vite |
| UI components | [Skeleton UI v3](https://www.skeleton.dev) on Tailwind |
| Editor | [Tiptap](https://tiptap.dev) (ProseMirror) |
| At-rest encryption | SQLCipher (AES-256) with vendored OpenSSL |
| Platforms | Windows, macOS, Linux |

### Project structure

```
unkai-mail/
├── Cargo.toml              # Rust workspace root
├── crates/
│   ├── unkai-core/        # Shared types, models, error handling
│   ├── unkai-imap/        # IMAP mail retrieval
│   ├── unkai-smtp/        # SMTP mail sending
│   ├── unkai-jmap/        # JMAP modern mail access
│   ├── unkai-caldav/      # CalDAV calendar sync
│   ├── unkai-carddav/     # CardDAV contact sync
│   ├── unkai-discovery/   # Mozilla autoconfig + DNS SRV discovery
│   ├── unkai-nextcloud/   # Nextcloud OCS API (Talk, Files, …)
│   └── unkai-store/       # Local cache + encrypted SQLite + keychain
├── src-tauri/              # Tauri shell (entry point + capabilities)
└── ui/                     # Svelte 5 + TypeScript + Vite
    └── src/lib/            # Components
```

Each protocol is its own crate so it's testable and swappable. The Tauri
layer is deliberately thin — it exposes commands; all logic lives in the
Rust core.

---

## Getting started

### Prerequisites

- **Rust** (stable, edition 2024) — install via [rustup](https://rustup.rs)
- **Node.js** 20+ and npm
- **Tauri system dependencies** — see the
  [Tauri prerequisites guide](https://v2.tauri.app/start/prerequisites/)
  for your OS (WebView2 on Windows, WebKitGTK on Linux, etc.)
- **`cargo tauri` CLI** — `cargo install tauri-cli`
- **Windows only: Strawberry Perl** — SQLCipher's vendored OpenSSL build
  needs a full Perl install:
  ```powershell
  winget install StrawberryPerl.StrawberryPerl
  ```
  Then make sure Strawberry Perl is on `PATH` before Git's Perl. End
  users don't need Perl; OpenSSL is statically linked into the shipped
  binary.

### Run in dev mode

```bash
cd ui && npm install && cd ..
cargo tauri dev
```

On first launch you'll see the account setup wizard. Enter IMAP / SMTP
server details and password (stored in your OS keychain, not on disk).
Connect Nextcloud separately from Settings — Unkai opens a browser-based
login that returns a revocable app password.

<!--
  GIF: account-setup
  ~8 s capture of the AccountSetup wizard: email field with auto-discovery
  hint after blur, then advancing through the IMAP/SMTP steps to a
  successful "test connection" green tick.
-->
<p align="center">
  <em>🎬 Animated demo placeholder — first-launch account setup wizard</em>
</p>

### Build a release

```bash
cargo tauri build
```

Installer / bundle for your platform lands in
`src-tauri/target/release/bundle/`.

### Tests & lint

```bash
cargo test --workspace        # All Rust tests
cargo clippy --workspace      # Lint
cd ui && npm run check        # Svelte / TypeScript type-check
```

---

## Nextcloud connection

Nextcloud is independent of mail accounts — one server can back any
number of IMAP/SMTP identities. From Settings → Nextcloud, enter your
server URL and click *Connect*:

1. Unkai opens your system browser at the Nextcloud login page.
2. You authorise Unkai there. Any IdP / SSO in front of Nextcloud
   (Keycloak, Authelia, Entra ID, …) works because the login happens
   in the browser, not inside Unkai.
3. Nextcloud generates a **revocable app password** and hands it back;
   Unkai stores it in the OS keychain. You can revoke Unkai at any
   time from Nextcloud → Personal → Security without changing your
   real password.

Once connected, Unkai probes `/ocs/v2.php/cloud/capabilities` and
shows which Nextcloud apps are available (Talk, Files, Calendar,
Contacts, Office, Notes).

<!--
  SCREENSHOT: nextcloud-settings
  Settings → Nextcloud panel with one connected account showing the
  capability chips (Talk / Files / Calendar / Contacts / Office) and
  the "Trust server cert" button.
-->
<p align="center">
  <em>📸 Screenshot placeholder — Nextcloud connection panel</em>
</p>

---

## Theming

Unkai uses [Skeleton UI](https://www.skeleton.dev) for theming. You can
pick any of Skeleton's 22 stock themes from *Settings → Design* plus a
Light / Dark / Follow-OS toggle. Custom CSS themes from Skeleton's
[Theme Generator](https://themes.skeleton.dev) (or any third-party
Skeleton-shaped CSS file) can be imported via *+ Import theme…*.

App-icon styles (Storm, Dawn, Mint, Sky, Twilight, monochrome black /
white, plus the v2 Copper / Forest / Midnight / Ocean / Rose / Slate /
Sunset set) live in the same panel — pick once, the tray, window
titlebar, and Windows taskbar entry update immediately.

---

## Architecture principles

- **Separation of concerns** — Rust core handles all protocol /
  business logic; the UI is a thin presentation layer.
- **Offline-first** — encrypted local cache so the client works without
  constant connectivity.
- **Security-first** — TLS everywhere, OS-keychain credentials, no
  plaintext secrets on disk.
- **Modular design** — every protocol is its own crate.
- **Stay responsive** — heavy work goes on async background tasks,
  never on the UI thread.

---

## Roadmap

Tracked in [GitHub Issues](https://github.com/Videothek/unkai-mail/issues).

**Done**
- IMAP: connect, list folders, fetch envelopes + full messages
- SMTP: send messages with rich-text + attachments
- Encrypted local cache via SQLCipher, OS-keychain master key
- Account setup wizard with IMAP/SMTP probe + autodiscovery
- Nextcloud: browser-based login (Login Flow v2) + capability detection
- Nextcloud Files: attach, share with password, embedded Office viewer
- Nextcloud Talk: room creation from compose, auto-attach join link
- CalDAV: full calendar view, event creation, iMIP RSVP
- CardDAV: contact view, mailing lists, @-mentions in compose
- Full-text search over the encrypted cache (operator syntax + filters)
- Infinite scroll for older mails / search results
- System tray + desktop notifications
- Skeleton theme picker + custom theme import
- App-icon picker (14 styles) with hot-swap

**Next up**
- End-to-end mail encryption (S/MIME + OpenPGP)
- Calendar invites + RSVP polish
- AI-assisted reply drafting + RAG over mail
- Drag-and-drop messages between folders
- HTML body renderer with per-sender remote-image trust

**Later**
- JMAP support (the crate exists; runtime is partial)
- Spam / phishing classification
- Cross-client interop for `@`-mentions and `/`-attachment refs

---

## Contributing

This is a two-person project (Nick and Jannik) in early development, but
issues and pull requests are welcome.

- `main` is stable and always compiles.
- Feature work happens on short-lived branches named
  `feature/<issue-number>-<slug>`, branched from current `main` and
  merged via PR.
- Never push directly to `main`.

`CLAUDE.md` in the repo root captures the working context document used
during AI-assisted development — read it for the full set of conventions.

---

## License

Unkai Mail itself is [GPL-3.0](LICENSE).

- [`SBOM.md`](SBOM.md) — direct-dependency inventory + licence cheat-sheet
  + what each licence forces our distribution model to look like.
- [`License.md`](License.md) — third-party attribution document
  (the legal-notice rollup we ship next to binaries).

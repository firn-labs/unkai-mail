# Software Bill of Materials (SBOM)

Inventory of every direct dependency Unkai Mail pulls in, the licence
each one ships under, and what that combination means for distributing
the app commercially.

> **TL;DR:** Unkai is licensed **GPL-3.0**. The strongest copyleft
> dependency in the tree (`rrule`, GPL-3.0) is what forces that choice —
> we can market and sell the app, but we must offer the source to every
> user we ship a binary to. The public GitHub repository already
> satisfies that obligation. Dropping `rrule` would let us relicense
> down to LGPL-3.0 or weaker; everything else is permissive.

---

## Licence cheat-sheet (only the marketing-relevant facts)

The list below covers only the licences that actually appear in this
project's tree. We're not exhaustive on the OSI catalogue — just the
ones that affect what we're allowed to do.

### Permissive (no commercial restrictions)

These let us do **anything**: ship binaries, sell them, embed them,
keep our own code closed, modify and redistribute. The only hard
requirement is that we preserve the licence notice and copyright
attribution somewhere users can see (typically a "Third-party licences"
screen or a `LICENSES.md` shipped with the binary).

- **MIT** — preserve copyright + permission notice.
- **Apache-2.0** — preserve copyright + permission notice + the
  `NOTICE` file if upstream ships one. Includes an explicit patent
  grant: contributors to the dep can't sue us for using their patents
  in the way the dep uses them.
- **BSD-2-Clause / BSD-3-Clause** — same shape as MIT, with the extra
  "no endorsement" clause in 3-Clause (we may not use upstream's name
  to promote derivative work without permission).
- **ISC** — functionally MIT with simpler wording.
- **0BSD / Unlicense / CC0** — public-domain equivalents. No attribution
  required at all.
- **Zlib** — permissive with one extra clause (don't claim you wrote
  the original). Treat like MIT.

For Unkai this means: anything pure-permissive can ship in any
distribution model we pick.

### Weak copyleft (must remain replaceable)

- **MPL-2.0** (Mozilla Public License 2.0) — file-level copyleft.
  Modifications to MPL-licensed *files* must be released under MPL,
  but new files we add ourselves stay under whatever licence we
  pick. We can ship a closed-source binary as long as the MPL files
  are dynamically replaceable (or at minimum: source is available
  for those files). Not a problem in practice — we don't modify our
  MPL deps, we just consume them.
- **LGPL-3.0** (Lesser GPL) — library-level copyleft. We can use an
  LGPL library in a closed-source app **only if** users can replace
  the LGPL library with their own modified version. For statically
  linked Rust crates that requires shipping object files or a build
  script. We currently have no LGPL-3.0 deps; flag if you add one.

### Strong copyleft (forces our app's licence)

- **GPL-3.0 / AGPL-3.0** — viral. Linking, statically or dynamically,
  to a GPL-3.0 library forces our combined work to also be distributed
  under GPL-3.0 (or a compatible licence). We can absolutely sell the
  binary commercially — there's no royalty-free clause — but every
  user we sell to has the right to a copy of the complete source
  (`src/` plus our build instructions). AGPL adds an extra trigger: if
  the software runs as a network service, anyone interacting with it
  over the network is also a "user" entitled to the source. Unkai is
  a desktop client, not a service, so AGPL would behave like GPL for
  our distribution model.

For Unkai this is what `rrule` brings in — and is why our own
licence is GPL-3.0. **Adding AGPL-3.0 anywhere in the tree would
upgrade the obligation** (network-service trigger), so flag carefully.

### Dual-licensed deps

Several Rust crates ship under `MIT OR Apache-2.0` (also written as
`MIT/Apache-2.0`) — we may pick whichever licence we prefer when
redistributing. In practice we keep both notices because they're
trivial.

A handful pick `MIT OR Apache-2.0 OR Zlib` or similar; the analysis
above still applies — pick the one that fits.

### What "GPL-3.0 forces our licence" means in practice

- ✅ We can sell Unkai binaries.
- ✅ We can run paid hosting / support / consulting around it.
- ✅ We can make the source available *only* on the GitHub repo;
  the repo URL counts as "offer to provide source".
- ❌ We cannot ship a closed-source proprietary fork.
- ❌ We cannot dual-license Unkai under a non-GPL licence without
  swapping or relicensing every GPL dep first.
- ❌ We cannot add code under a licence incompatible with GPL-3.0
  (e.g. older GPL-2.0-only, BUSL, SSPL).

---

## Maintenance rule

This file must be updated **every time a dependency is added, removed,
or upgraded** — both in `Cargo.toml` (workspace + per-crate) and in
`ui/package.json`. The companion file [`License.md`](License.md) must
be updated in lockstep: it carries the actual licence-notice text for
attribution when we ship binaries.

Any new dependency must have its licence verified; introducing a
strong-copyleft licence stronger than what we already have (e.g.
AGPL-3.0) is a project-level decision, not a routine PR. See
[CLAUDE.md](CLAUDE.md) for the AI-assistant version of this rule.

Last manual reconciliation: 2026-05-15 (`tauri-plugin-deep-link` + `tauri-plugin-single-instance` added so Unkai can register as the OS mailto handler and de-dup second-instance launches, #294 — both MIT OR Apache-2.0, no licence pressure).

---

## Rust dependencies (workspace)

Direct dependencies declared in the workspace `Cargo.toml`. Transitive
deps are governed by the strongest licence reached in the chain — for
us, that's GPL-3.0 via `rrule`.

| Package | Licence | Notes |
|---|---|---|
| `tokio` | MIT | Async runtime. |
| `serde` / `serde_json` | MIT OR Apache-2.0 | Serialization. |
| `thiserror` | MIT OR Apache-2.0 | Error-derive macro. |
| `anyhow` | MIT OR Apache-2.0 | Generic error type. |
| `tracing` / `tracing-subscriber` | MIT | Structured logging. |
| `reqwest` | MIT OR Apache-2.0 | HTTP client. |
| `chrono` | MIT OR Apache-2.0 | Date / time. |
| `chrono-tz` | MIT OR Apache-2.0 | IANA tz database (bundled). |
| **`rrule`** | **GPL-3.0** | **RFC 5545 recurrence-rule engine.** This is the dep that forces our project licence. |
| `dirs` | MIT OR Apache-2.0 | Per-OS config / data paths. |
| `keyring` | MIT OR Apache-2.0 | OS keychain access. |
| `rusqlite` | MIT | SQLite bindings (bundled SQLCipher build). |
| `r2d2` / `r2d2_sqlite` | MIT OR Apache-2.0 | Connection pool. |
| `getrandom` | MIT OR Apache-2.0 | OS-cryptographic RNG. |
| `open` | MIT OR Apache-2.0 | "Open in default app" cross-platform. |
| `hex` | MIT OR Apache-2.0 | Hex encoding. |
| `async-imap` | Apache-2.0 OR MIT | Async IMAP client. |
| `futures` | MIT OR Apache-2.0 | Async primitives. |
| `rustls` | Apache-2.0 OR ISC OR MIT | TLS (ring backend). |
| `tokio-rustls` | MIT OR Apache-2.0 | Tokio adapter for rustls. |
| `webpki-roots` | MPL-2.0 | Mozilla root CA bundle. File-level copyleft; we don't modify it. |
| `rustls-pki-types` | MIT OR Apache-2.0 | TLS type primitives. |
| `tokio-util` | MIT | Tokio compat shims. |
| `sha2` | MIT OR Apache-2.0 | SHA-256 (cert fingerprint display). |
| `lettre` | MIT OR Apache-2.0 | SMTP client. |
| `mail-parser` | Apache-2.0 OR MIT | RFC 5322 / MIME parser. |
| `quick-xml` | MIT | XML parser (CalDAV / CardDAV). |
| `ical` | Apache-2.0 OR MIT | iCalendar / vCard parsing. |
| `base64` | MIT OR Apache-2.0 | Base64 codec. |
| `aes-gcm` | MIT OR Apache-2.0 | AES-256-GCM (encrypted cache). |
| `pbkdf2` | MIT OR Apache-2.0 | Key derivation. |
| `hmac` | MIT OR Apache-2.0 | HMAC primitive. |
| `uuid` | MIT OR Apache-2.0 | UUID generation. |
| `hickory-resolver` | MIT OR Apache-2.0 | DNS resolver (autoconfig SRV lookup). |
| `font-kit` | MIT OR Apache-2.0 | System font enumeration. |

### Tauri shell (`src-tauri/Cargo.toml`)

| Package | Licence | Notes |
|---|---|---|
| `tauri` (v2) | MIT OR Apache-2.0 | Desktop shell framework. |
| `tauri-build` | MIT OR Apache-2.0 | Build-time helper. |
| `tauri-plugin-notification` | MIT OR Apache-2.0 | OS notifications. |
| `tauri-plugin-dialog` | MIT OR Apache-2.0 | Native file dialogs. |
| `tauri-plugin-autostart` | MIT OR Apache-2.0 | Run-on-login registration. |
| `tauri-plugin-deep-link` | MIT OR Apache-2.0 | `mailto:` protocol-handler registration (#294). |
| `tauri-plugin-single-instance` | MIT OR Apache-2.0 | Forwards a second-launch `mailto:` argv to the running instance (#294). |
| `notify-rust` | MIT OR Apache-2.0 | Cross-platform desktop notifications. |
| `windows` (winapi) | MIT OR Apache-2.0 | Windows API bindings (taskbar overlay). |

**Indirect / vendored**:
- **SQLCipher** (community edition, vendored through `rusqlite`'s
  `bundled-sqlcipher-vendored-openssl` feature) — **BSD-3-Clause**.
  Permissive, no impact on our licence.
- **OpenSSL** (vendored) — **Apache-2.0**. Same.
- **ring** (TLS crypto provider) — **ISC + MIT + OpenSSL** (their
  custom mix). Permissive enough that no obligation flows back.

---

## UI dependencies (`ui/package.json`)

### Runtime (`dependencies`)

| Package | Licence | Notes |
|---|---|---|
| `@tauri-apps/api` | MIT OR Apache-2.0 | Tauri JS bridge. |
| `@tauri-apps/plugin-autostart` | MIT OR Apache-2.0 | JS side of the autostart plugin. |
| `@tauri-apps/plugin-dialog` | MIT OR Apache-2.0 | JS side of the dialog plugin. |
| `@tauri-apps/plugin-notification` | MIT OR Apache-2.0 | JS side of the notification plugin. |
| `@tiptap/core` and `@tiptap/extension-*` | MIT | Rich-text editor (we use ~15 extensions). |
| `@tiptap/pm` | MIT | ProseMirror runtime bundled by Tiptap. |
| `@tiptap/starter-kit` | MIT | Default Tiptap node bundle. |
| `@tiptap/suggestion` | MIT | Mention / autocomplete plugin. |
| `codemirror` and `@codemirror/*` | MIT | Notes markdown editor (#138) — meta package + grammar / state / view / autocomplete sub-packages. |
| `marked` | MIT | Markdown → HTML renderer for the Notes preview pane (#138). |
| `dompurify` | MPL-2.0 OR Apache-2.0 | HTML sanitiser for inbound mail bodies. We can pick MPL or Apache; either way no licence pressure on our app. |
| `emoji-picker-element` | MIT | Emoji picker web component. |
| `svelte-tiptap` | MIT | Svelte wrapper around Tiptap. |
| `@inlang/paraglide-js` | Apache-2.0 | i18n compiler (#190). Generates per-locale message modules at build time; the runtime helper that ships in the bundle is a small selection-only function. |

### Build / type-check (`devDependencies`)

These run at build time but don't ship in the binary, so their licences
don't affect distribution. Still worth knowing what's in the toolchain:

| Package | Licence | Notes |
|---|---|---|
| `@skeletonlabs/skeleton` | MIT | Skeleton UI core. |
| `@skeletonlabs/skeleton-svelte` | MIT | Skeleton's Svelte adapter. |
| `@sveltejs/vite-plugin-svelte` | MIT | Vite ↔ Svelte glue. |
| `@tailwindcss/typography` | MIT | Tailwind prose plugin. |
| `@tailwindcss/vite` | MIT | Tailwind Vite integration. |
| `@tsconfig/svelte` | MIT | Stock TS config for Svelte. |
| `@types/dompurify` | MIT | TS types. |
| `@types/node` | MIT | TS types for Node. |
| `svelte` | MIT | Svelte compiler / runtime. |
| `svelte-check` | MIT | Type-check tool. |
| `tailwindcss` | MIT | CSS framework. |
| `typescript` | Apache-2.0 | TS compiler. |
| `vite` | MIT | Build tool / dev server. |
| `vitest` | MIT | Frontend unit-test runner (#295). Runs pure-function tests via `npm test`; not wired into CI yet — local-only smoke for now. |

---

## Runtime data feeds

Not a code dependency — **data** consumed at runtime. Each feed
needs the same kind of attention as a code dep when its licence
or terms change.

| Source | Licence | Notes |
|---|---|---|
| URLhaus by abuse.ch (`urlhaus.abuse.ch/downloads/csv_online/`) | CC0-1.0 | Malicious-URL feed for the link-safety check (#165). Fetched once an hour over HTTPS, stored in the encrypted SQLCipher cache. Public domain — no attribution clause forces redistribution semantics, but we still credit abuse.ch in the Settings UI as a goodwill gesture. |

---

## Distribution implications, summarised

| Distribution model | Permitted today | Why |
|---|---|---|
| Sell binaries, GitHub repo public | ✅ | GPL-3.0 binary + source available = compliant. |
| Free download from GitHub releases | ✅ | Same. |
| Bundle into a paid SaaS / hosted offering | ⚠️ | Allowed under GPL-3.0, but if we add an AGPL-3.0 dep we'd also have to expose source via the running service. |
| Closed-source proprietary fork | ❌ | GPL-3.0 from `rrule` blocks this. |
| Dual-licence under e.g. commercial + GPL | ❌ | Same. Would need to swap `rrule` for an MIT/Apache RRULE expander. |
| Ship in a closed-source company-internal tool only | ✅ | GPL-3.0's redistribution clause only triggers on distribution. Internal use is unrestricted. |

If at some point we want the option of relicensing Unkai to a
permissive licence (MIT / Apache / a commercial dual-licence), the
single hard blocker is `rrule`. It would need replacing — either by
forking it under a permissive licence (which is itself a GPL violation
unless the upstream rights-holders agree) or by writing / sourcing an
RFC 5545 expander under MIT / Apache. None of the other deps in the
tree force anything stronger than weak copyleft.

# Third-Party Licenses

Nimbus Mail is licensed under [**GPL-3.0**](LICENSE). The compiled
binary bundles, statically links, or otherwise relies on the
third-party software listed below. Each item is governed by its
own license, reproduced or referenced inline.

This file is the **legal attribution document** — what we ship next
to the binary so each upstream's notice obligations are met. For the
broader discussion of what each licence type forces our distribution
model to look like (sell binaries / closed-source fork / dual-licence
options), see [SBOM.md](SBOM.md).

The full direct-dependency inventory is in [SBOM.md](SBOM.md); this
file groups the same packages by licence so that each notice block
applies to every package listed underneath it.

---

## Quick index

| Licence | Where it appears |
|---|---|
| [MIT](#mit-license) | most Rust crates, all UI runtime / dev deps |
| [Apache-2.0](#apache-license-20) | dual-licensed Rust crates, OpenSSL (vendored), TypeScript |
| [BSD-3-Clause](#bsd-3-clause-license) | SQLCipher (vendored) |
| [ISC](#isc-license) | rustls, ring (parts) |
| [MPL-2.0](#mozilla-public-license-20) | webpki-roots, dompurify |
| [GPL-3.0](#gnu-general-public-license-v3) | rrule (and Nimbus itself) |
| [CC0-1.0](#cc0-10-runtime-data-feeds) (data only) | URLhaus malicious-URL feed (#165) |
| Multi-licence components | [ring](#ring-tls-crypto-provider) (ISC + MIT + OpenSSL) |

---

## MIT License

The following packages are distributed under the MIT License:

**Rust crates**: `tokio`, `serde`, `serde_json`, `thiserror`, `anyhow`,
`tracing`, `tracing-subscriber`, `reqwest`, `chrono`, `chrono-tz`,
`dirs`, `keyring`, `rusqlite`, `r2d2`, `r2d2_sqlite`, `getrandom`,
`open`, `hex`, `async-imap`, `futures`, `tokio-rustls`,
`rustls-pki-types`, `tokio-util`, `sha2`, `lettre`, `mail-parser`,
`quick-xml`, `ical`, `base64`, `aes-gcm`, `pbkdf2`, `hmac`, `uuid`,
`hickory-resolver`, `font-kit`, `tauri`, `tauri-build`,
`tauri-plugin-notification`, `tauri-plugin-dialog`,
`tauri-plugin-autostart`, `tauri-plugin-deep-link`,
`tauri-plugin-single-instance`, `notify-rust`, `windows`.

**npm packages**: `@tauri-apps/api`, `@tauri-apps/plugin-autostart`,
`@tauri-apps/plugin-dialog`, `@tauri-apps/plugin-notification`,
`@tiptap/core` and every `@tiptap/extension-*`, `@tiptap/pm`,
`@tiptap/starter-kit`, `@tiptap/suggestion`, `emoji-picker-element`,
`svelte-tiptap`, `codemirror`, `@codemirror/lang-markdown`,
`@codemirror/state`, `@codemirror/view`, `@codemirror/commands`,
`@codemirror/language`, `@codemirror/autocomplete`, `marked`,
`@skeletonlabs/skeleton`,
`@skeletonlabs/skeleton-svelte`, `@sveltejs/vite-plugin-svelte`,
`@tailwindcss/typography`, `@tailwindcss/vite`, `@tsconfig/svelte`,
`@types/dompurify`, `@types/node`, `@inlang/paraglide-js`,
`svelte`, `svelte-check`, `tailwindcss`, `vite`, `vitest`.

Several of these are dual-licensed `MIT OR Apache-2.0`; we list each
package only under its first applicable section here.

```
MIT License

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

The copyright notice for each individual package is preserved in its
upstream source repository and reproduced verbatim in the package's
`LICENSE` / `LICENSE-MIT` file as fetched by Cargo / npm during
build.

---

## Apache License 2.0

The following are available under Apache-2.0 (either exclusively or as
the alternate half of an `MIT OR Apache-2.0` dual-licence — we may
choose either, and choose Apache-2.0 here for transitive compatibility
with downstream consumers who require it):

**Rust crates**: `serde`, `thiserror`, `anyhow`, `reqwest`, `chrono`,
`chrono-tz`, `dirs`, `keyring`, `r2d2`, `r2d2_sqlite`, `getrandom`,
`open`, `hex`, `async-imap`, `futures`, `rustls`, `tokio-rustls`,
`rustls-pki-types`, `mail-parser`, `ical`, `base64`, `aes-gcm`,
`pbkdf2`, `hmac`, `uuid`, `hickory-resolver`, `font-kit`, `tauri`,
`tauri-build`, `tauri-plugin-notification`, `tauri-plugin-dialog`,
`tauri-plugin-autostart`, `tauri-plugin-deep-link`,
`tauri-plugin-single-instance`, `notify-rust`, `windows`.

**Vendored libraries**: OpenSSL (statically linked through
`rusqlite`'s `bundled-sqlcipher-vendored-openssl` feature).

**npm packages**: `dompurify` (alternate half of `MPL-2.0 OR
Apache-2.0`), `typescript`.

The full Apache License 2.0 text is reproduced at:
<https://www.apache.org/licenses/LICENSE-2.0.txt>

Apache-2.0 requires:
- Reproduction of the licence and any `NOTICE` file from the
  upstream package alongside the binary.
- Preservation of all copyright, patent, trademark, and attribution
  notices.
- Marking modified files with a "Modified by" notice.

When we ship a binary bundle, this `License.md` file together with
each upstream's preserved `LICENSE` / `NOTICE` text inside its source
package satisfies the reproduction requirement.

---

## BSD-3-Clause License

**Vendored library**: SQLCipher Community Edition — statically linked
into the binary via `rusqlite`'s
`bundled-sqlcipher-vendored-openssl` feature. Provides the encrypted
local mail cache.

```
Copyright (c) Zetetic LLC. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the
   distribution.
3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived
   from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

The Nimbus Mail project is not affiliated with or endorsed by Zetetic
LLC. Per clause 3 of the BSD-3-Clause licence, we do not use Zetetic's
name to promote Nimbus.

---

## ISC License

**Rust crates**: `rustls` (alternate half of `Apache-2.0 OR ISC OR
MIT`).

The ISC licence is functionally identical to the simplified BSD /
MIT permissive model with abbreviated wording.

```
Permission to use, copy, modify, and/or distribute this software for
any purpose with or without fee is hereby granted, provided that the
above copyright notice and this permission notice appear in all
copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL
WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE
AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL
DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR
PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

---

## Mozilla Public License 2.0

The following are governed by MPL-2.0:

- **`webpki-roots`** (Rust crate) — Mozilla's curated list of trusted
  root CAs, bundled into the rustls trust store. Source:
  <https://github.com/rustls/webpki-roots>
- **`dompurify`** (npm package) — HTML sanitiser used by the mail
  reader. Available as `MPL-2.0 OR Apache-2.0`; we honour the MPL-2.0
  obligations as the more conservative choice. Source:
  <https://github.com/cure53/DOMPurify>

MPL-2.0 is **file-level weak copyleft**. Modifications to MPL-licensed
*files* must be released under MPL, but new files we add ourselves
stay under our own licence. We do not modify either of these
dependencies — we consume them as published — so we do not generate
any source obligations beyond preserving the original licence headers,
which Cargo / npm already do at install time.

If you ship a binary built from this repository, you must:
- Make the source of the MPL-licensed files available on request.
- Preserve every copyright notice in the upstream source archive.

The full MPL-2.0 text is at: <https://www.mozilla.org/en-US/MPL/2.0/>

---

## GNU General Public License v3

- **`rrule`** (Rust crate) — RFC 5545 recurrence-rule expansion engine,
  used by `nimbus-caldav` to materialise recurring calendar events
  into concrete occurrences. Source:
  <https://github.com/fmeringdal/rust-rrule>
- **Nimbus Mail itself** is licensed under GPL-3.0 (see [LICENSE](LICENSE)
  in the repository root). The presence of `rrule` in the dependency
  tree is the load-bearing reason — every binary we distribute must be
  accompanied by source under GPL-3.0 or a compatible licence.

GPL-3.0 is **strong viral copyleft**. Distributing a binary that
links to GPL-3.0 code requires:
- Distributing the complete corresponding source, or making it
  available via a written offer that any recipient can act on for at
  least three years. Our public GitHub repository at
  <https://github.com/Videothek/nimbus-mail> serves as that
  availability.
- Preserving copyright notices.
- Licensing any work that combines with GPL-3.0 code under GPL-3.0
  itself.

The full GPL-3.0 text is in the repository root [LICENSE](LICENSE)
file and at <https://www.gnu.org/licenses/gpl-3.0.txt>.

---

## CC0-1.0 (runtime data feeds)

The link-safety check (#165) consumes the **URLhaus** malicious-URL
feed published by [abuse.ch](https://urlhaus.abuse.ch/). The data
itself is dedicated to the public domain under
[CC0-1.0](https://creativecommons.org/publicdomain/zero/1.0/) — no
attribution clause forces redistribution semantics, and no
copyleft pressure spreads to the rest of Nimbus. We still credit
abuse.ch in the Settings UI as a goodwill gesture: the project
runs on community contributions and a visible "powered by" link
helps them keep funding the work.

The CC0-1.0 dedication does not impose any reproduction or notice
obligation. Reproduced here for completeness:

> CC0 1.0 Universal — The person who associated a work with this
> deed has dedicated the work to the public domain by waiving all
> of his or her rights to the work worldwide under copyright law,
> including all related and neighboring rights, to the extent
> allowed by law.

Full text: <https://creativecommons.org/publicdomain/zero/1.0/legalcode>

---

## ring (TLS crypto provider)

`ring` is the cryptographic primitives library backing `rustls` (our
TLS implementation). It carries a custom multi-licence notice:

> ISC-style license with portions under Apache-2.0, BSD-style, and the
> OpenSSL Project's licence (the latter inherited from BoringSSL
> code).

The full notice is reproduced in the `ring` source archive at
<https://github.com/briansmith/ring/blob/main/LICENSE> and is shipped
inside the package's source distribution. By transitively including
all the constituent licences (ISC / BSD / Apache-2.0 / OpenSSL / SSLeay)
we satisfy the attribution obligation; none of these licences are
copyleft, so they impose no licence pressure on Nimbus beyond
attribution preservation.

---

## How attribution is shipped

For source distribution (this repository) the relevant licence text
travels with each package's source under `~/.cargo/registry/src/...`
or `node_modules/...` after `cargo build` / `npm install`. No
additional bundling is required.

For binary releases (`cargo tauri build`), this `License.md` file
plus the per-package licences extracted by Cargo / npm at build time
are bundled inside the installer's `licenses/` directory (TODO when
the release pipeline is set up). The combined result satisfies every
upstream's reproduction-of-notice requirement.

---

## Updating this file

Whenever a dependency is added, removed, or upgraded:

1. Update [SBOM.md](SBOM.md) (the inventory + marketing-implications
   document — see the maintenance rule there).
2. Update this file: add the package to the appropriate licence
   section above, or create a new section if the licence isn't
   represented yet.
3. If the new dependency introduces a stronger copyleft licence than
   what's already present (e.g. AGPL-3.0), surface that as a project-
   level decision before merging — it changes our distribution model.

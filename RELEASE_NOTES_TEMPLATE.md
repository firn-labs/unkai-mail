<!--
  Editorial template for a Unkai Mail release.

  When the release workflow runs (`v*` tag pushed), it creates a
  *draft* GitHub Release with the auto-generated changelog from
  every PR merged since the previous tag (categories defined in
  `.github/release.yml`).

  Open the draft, paste this template above the auto-generated
  changelog, fill in the editorial sections, then publish.

  Suggested rule of thumb for what goes in each section:
    * Headline — one sentence that someone seeing the release
      list can read at a glance.
    * What's new — features the user notices.  Skip refactors
      and dependency bumps; those are in the auto changelog.
    * Bug fixes — user-visible fixes only.  Internal cleanups
      do not belong here.
    * Known issues — anything you want users to be aware of
      before installing.  Empty is fine; do not invent issues
      to fill space.
    * Upgrade notes — only when the user has to do something
      manual (re-login, clear cache, re-export config, etc.).

  Delete this comment block before publishing.
-->

# Unkai Mail vX.Y.Z — _<one-sentence headline>_

## ✨ What's new

- _Feature one — what changed and why a user should care._
- _Feature two._

## 🐛 Bug fixes

- _Fix one — described from the user's point of view ("the sidebar no longer freezes when …")._
- _Fix two._

## ⚠️ Known issues

- _If empty, delete this section entirely. Do not list "no known issues"._

## 🔄 Upgrade notes

- _Only include if the user has to take a manual step (re-authenticate, re-import config, clear a cache, etc.). Otherwise delete this section._

---

## 💾 Install

| Platform | Download |
|---|---|
| Windows (10 / 11, x86_64) | `Unkai-Mail_X.Y.Z_x64-setup.exe` or `Unkai-Mail_X.Y.Z_x64_en-US.msi` |
| macOS (Apple Silicon) | `Unkai-Mail_X.Y.Z_aarch64.dmg` |
| Linux (Fedora / RHEL, x86_64) | `Unkai-Mail-X.Y.Z-1.x86_64.rpm` |
| Linux (Ubuntu / Debian, x86_64) | `Unkai-Mail_X.Y.Z_amd64.deb` |
| Linux (any distro, x86_64) | `Unkai-Mail_X.Y.Z_amd64.AppImage` |

> **No Intel Mac build yet:** releases ship Apple-Silicon (`aarch64`) macOS only — `macos-latest` CI runners are arm64. Intel-Mac users need an x64 build we don't yet produce. Delete this note once we add an x64 macOS matrix entry.

> **Note on signing:** these builds are not yet signed. Windows SmartScreen and macOS Gatekeeper will warn the first time you run the installer. We will start shipping signed builds once we provision the certs (Apple Developer ID + Windows EV Code Signing).

---

<!--
  Below this line is the auto-generated changelog from PRs.
  See `.github/release.yml` for how PRs are routed into the
  categories.  Bot-authored Dependabot PRs land in their own
  bucket so the headline list stays human-curated.
-->

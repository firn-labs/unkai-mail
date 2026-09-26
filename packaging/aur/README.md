# AUR packaging

Source of truth for the [`unkai-mail-bin`](https://aur.archlinux.org/packages/unkai-mail-bin)
package on the Arch User Repository (#602, part of #600).

| Path | What |
|---|---|
| `unkai-mail-bin/PKGBUILD` | Template — `@VERSION@` / `@SHA256@` are filled per release |
| `render.sh` | Renders the template for one tag (downloads the `.deb` for its sha256) |
| `../../.github/workflows/aur.yml` | Validates with `makepkg` in an Arch container; pushes to AUR on `release: published` |

> **Status (2026-09):** AUR account registration has been closed by the
> Arch team since the mid-2026 malware wave, so the publish job is
> skipped until `AUR_SSH_PRIVATE_KEY` exists. Arch users get the same
> package from the signed pacman repository at
> https://firn-labs.github.io/unkai-packages/ (built by
> [firn-labs/unkai-packages](https://github.com/firn-labs/unkai-packages)
> from this PKGBUILD) — see `docs/INSTALL.md`.

## How a release reaches AUR

1. `release.yml` builds the `.deb` and attaches it to a **draft** Release.
2. A human publishes the Release. GitHub fires `release: published`.
3. `aur.yml` renders the PKGBUILD for that tag, builds it with `makepkg -s`
   in an `archlinux:base-devel` container (so a broken package can never
   reach AUR), generates `.SRCINFO`, and pushes both to
   `ssh://aur@aur.archlinux.org/unkai-mail-bin.git`.

Pre-releases are skipped. To re-publish (or bootstrap the AUR repo the
first time), run the workflow manually from the Actions tab with the tag.

## Local dry-run

```sh
packaging/aur/render.sh v0.5.0 /tmp/aur
cd /tmp/aur && makepkg -s          # on Arch
namcap PKGBUILD unkai-mail-bin-*.pkg.tar.zst
```

Without an Arch box, the same thing in Docker:

```sh
docker run --rm -v "$PWD/packaging/aur:/aur" archlinux:base-devel bash -c '
  pacman -Syu --noconfirm --needed sudo namcap >/dev/null &&
  useradd -m b && echo "b ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/b &&
  cp -r /aur /tmp/aur && chown -R b /tmp/aur &&
  cd /tmp/aur && sudo -u b ./render.sh v0.5.0 /tmp/aur/out &&
  cd out && sudo -u b makepkg -s --noconfirm && namcap PKGBUILD *.pkg.tar.zst'
```

## Secrets (repo → Settings → Secrets → Actions)

| Secret | Value |
|---|---|
| `AUR_USERNAME` | AUR account name (shown as the commit author on AUR) |
| `AUR_EMAIL` | E-mail for the AUR commits |
| `AUR_SSH_PRIVATE_KEY` | Private half of a dedicated ed25519 key whose public half is registered on the AUR account |

Keep the key in the team password store next to the minisign updater key.

## Why `-bin`

Building from source on the user's machine means compiling the whole
Rust workspace, the WebKitGTK bindings and the Svelte UI (node + cargo +
tauri-cli). Repackaging the already-built, already-signed `.deb` is what
nearly every Tauri app on AUR does; a from-source `unkai-mail` package
can be added later without touching this one (`provides=unkai-mail`).

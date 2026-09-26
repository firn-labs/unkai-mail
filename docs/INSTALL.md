# Installing Unkai Mail

Pick your platform. Every Linux path below ends with a package that your
package manager keeps up to date — the in-app updater then only tells
you that a new version exists.

- [Windows](#windows)
- [macOS](#macos)
- [Linux — Arch, CachyOS, Manjaro, EndeavourOS](#arch-linux-and-derivatives)
- [Linux — Debian, Ubuntu, Linux Mint, Pop!_OS](#debian-and-ubuntu)
- [Linux — Fedora, RHEL, AlmaLinux, Rocky, openSUSE](#fedora-rhel-and-opensuse)
- [Linux — AppImage (any distro)](#appimage)
- [Linux — Flatpak](#flatpak)
- [Verifying the signing key](#verifying-the-signing-key)
- [Updating](#updating)
- [Uninstalling](#uninstalling)

All downloads come from the
[GitHub Releases](https://github.com/firn-labs/unkai-mail/releases/latest)
page; the Linux repositories are served from
**https://firn-labs.github.io/packages/unkai/** and are signed by Firn Labs.

> **Not yet OS code-signed** ([#558](https://github.com/firn-labs/unkai-mail/issues/558)):
> Windows SmartScreen and macOS Gatekeeper show a publisher warning the
> first time you run the installer. The Linux repositories and the
> in-app updater are cryptographically signed regardless.

---

## Windows

Windows 10 / 11, x86_64. Download one of:

| File | Notes |
|---|---|
| `Unkai-Mail_X.Y.Z_x64-setup.exe` | NSIS installer — recommended, per-user install, no admin needed |
| `Unkai-Mail_X.Y.Z_x64_en-US.msi` | MSI for managed / scripted deployments |

Run it; the app registers itself for `mailto:` links and `.eml` / `.ics`
files. Updates arrive through the in-app updater (Settings → Updates).

---

## macOS

Apple Silicon (`aarch64`) only for now — there is no Intel build yet.

1. Download `Unkai-Mail_X.Y.Z_aarch64.dmg`.
2. Open it and drag **Unkai Mail** into *Applications*.
3. First launch: right-click → *Open* to get past Gatekeeper once.

Updates arrive through the in-app updater.

---

## Arch Linux and derivatives

Arch, CachyOS, Manjaro, EndeavourOS, Garuda — anything with `pacman`.

### Repository (recommended)

Import the packaging key, sign it locally, add the repository, install:

```sh
curl -fsSL https://firn-labs.github.io/packages/keys/firn-labs.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key 5DF44E3B0086BA2276623A2A5AFF7E6FCB4F0F46

sudo tee -a /etc/pacman.conf <<'EOF'

[unkai-mail]
SigLevel = Required DatabaseOptional
Server = https://firn-labs.github.io/packages/unkai/arch/$arch
EOF

sudo pacman -Syu unkai-mail-bin
```

Compare the fingerprint with the one in
[Verifying the signing key](#verifying-the-signing-key) before signing
it. From now on `pacman -Syu` updates Unkai Mail with the rest of the
system. The package is the release `.deb` repacked — the same build
Debian users get.

### AUR

`unkai-mail-bin` will also be on the AUR once the Arch team re-opens
account registration (closed since the mid-2026 malware wave). The
PKGBUILD is maintained in this repo under
[`packaging/aur/`](../packaging/aur/); until then you can build it
yourself:

```sh
git clone --depth 1 https://github.com/firn-labs/unkai-mail
unkai-mail/packaging/aur/render.sh vX.Y.Z /tmp/unkai-aur
cd /tmp/unkai-aur && makepkg -si
```

---

## Debian and Ubuntu

Debian 12+, Ubuntu 22.04+, Linux Mint 21+, Pop!_OS 22.04+ (amd64).

### Repository (recommended)

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://firn-labs.github.io/packages/keys/firn-labs.gpg | sudo tee /etc/apt/keyrings/firn-labs.gpg >/dev/null
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/firn-labs.gpg] https://firn-labs.github.io/packages/unkai/apt stable main" | sudo tee /etc/apt/sources.list.d/unkai-mail.list

sudo apt update && sudo apt install unkai-mail
```

`apt upgrade` now updates Unkai Mail like any other package.

### Single .deb

```sh
sudo apt install ./Unkai-Mail_X.Y.Z_amd64.deb
```

No automatic updates — repeat with the next release's file.

---

## Fedora, RHEL and openSUSE

Fedora 40+, RHEL / AlmaLinux / Rocky 9+, openSUSE Leap 15.6+ / Tumbleweed (x86_64).

### Repository (recommended)

Fedora 41+ (dnf 5):

```sh
sudo dnf config-manager addrepo --from-repofile=https://firn-labs.github.io/packages/unkai/rpm/unkai-mail.repo
sudo dnf install unkai-mail
```

RHEL / Alma / Rocky / Fedora 40 (dnf 4):

```sh
sudo dnf config-manager --add-repo https://firn-labs.github.io/packages/unkai/rpm/unkai-mail.repo
sudo dnf install unkai-mail
```

openSUSE:

```sh
sudo zypper addrepo https://firn-labs.github.io/packages/unkai/rpm/unkai-mail.repo
sudo zypper refresh && sudo zypper install unkai-mail
```

Both the packages and the repository metadata are signed; the first
install asks you to accept the key — compare its fingerprint below.

### Single .rpm

```sh
sudo dnf install ./Unkai-Mail-X.Y.Z-1.x86_64.rpm
```

---

## AppImage

Any x86_64 distro, no root needed. The AppImage is the one Linux
format that updates itself in-app.

```sh
chmod +x Unkai-Mail_X.Y.Z_amd64.AppImage
./Unkai-Mail_X.Y.Z_amd64.AppImage
```

Needs FUSE 2 (`libfuse2` on Ubuntu 22.04+). For a desktop entry, run it
through [AppImageLauncher](https://github.com/TheAssassin/AppImageLauncher)
or `Gear Lever`.

---

## Flatpak

Planned — tracked in [#604](https://github.com/firn-labs/unkai-mail/issues/604).
Until it lands on Flathub, use one of the repositories above.

---

## Verifying the signing key

All three Linux repositories are signed with one OpenPGP key held by
Firn Labs. Its fingerprint is published at
https://firn-labs.github.io/packages/unkai/#key and in
`https://firn-labs.github.io/packages/keys/FINGERPRINT`.
Check it against this document before trusting the key:

```
5DF4 4E3B 0086 BA22 7662 3A2A 5AFF 7E6F CB4F 0F46
```

Key: *Firn Labs Packaging \<packages@firn-labs.com\>*, ed25519, no expiry.

Release assets themselves (installers, AppImage, `.deb`, `.rpm`) carry a
`.sig` produced with the project's minisign key — that is what the
in-app updater verifies.

---

## Updating

| Install method | How updates arrive |
|---|---|
| Windows, macOS, AppImage | In-app updater (Settings → Updates); checks at startup and every 6 h, installs on your click |
| pacman repository | `sudo pacman -Syu` |
| apt repository | `sudo apt update && sudo apt upgrade` |
| dnf / zypper repository | `sudo dnf upgrade` / `sudo zypper update` |
| Single `.deb` / `.rpm`, AUR build | Manual — the app shows a "new version available" badge and points you here |

On package-manager installs the Download / Install buttons are hidden
in Settings → Updates; the app never replaces a file the package
manager owns.

---

## Uninstalling

| Platform | Command |
|---|---|
| Windows | Settings → Apps → Unkai Mail → Uninstall |
| macOS | Drag *Unkai Mail* from *Applications* to the Trash |
| Arch | `sudo pacman -Rs unkai-mail-bin` |
| Debian / Ubuntu | `sudo apt remove unkai-mail` |
| Fedora / RHEL | `sudo dnf remove unkai-mail` |
| openSUSE | `sudo zypper remove unkai-mail` |

Your mail cache, settings and profiles live in the OS config directory
(`%APPDATA%\unkai-mail`, `~/Library/Application Support/unkai-mail`,
`~/.config/unkai-mail`) and are **not** removed by the uninstaller —
delete that folder by hand if you want a clean slate. Account passwords
are in the OS keychain.

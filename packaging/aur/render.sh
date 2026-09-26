#!/usr/bin/env bash
# Render packaging/aur/unkai-mail-bin/PKGBUILD for one release (#602).
#
#   packaging/aur/render.sh v0.5.0 [out-dir]
#
# Downloads the release's .deb to compute its sha256, then fills the
# @VERSION@ / @SHA256@ placeholders.  The rendered PKGBUILD lands in
# <out-dir>/PKGBUILD (default: packaging/aur/out/).  Used by the
# `aur.yml` workflow and handy for a local `makepkg` dry-run:
#
#   packaging/aur/render.sh v0.5.0 /tmp/aur && cd /tmp/aur && makepkg -s
set -euo pipefail

tag="${1:?usage: render.sh vX.Y.Z [out-dir]}"
version="${tag#v}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${2:-$here/out}"
repo="firn-labs/unkai-mail"
asset="Unkai-Mail_${version}_amd64.deb"
url="https://github.com/${repo}/releases/download/${tag}/${asset}"

mkdir -p "$out"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "fetching ${url}" >&2
curl -sSfL --retry 3 -o "$tmp/$asset" "$url"
sha="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"

sed -e "s/@VERSION@/${version}/g" -e "s/@SHA256@/${sha}/g" \
  "$here/unkai-mail-bin/PKGBUILD" > "$out/PKGBUILD"

echo "rendered $out/PKGBUILD (pkgver=${version}, sha256=${sha})" >&2

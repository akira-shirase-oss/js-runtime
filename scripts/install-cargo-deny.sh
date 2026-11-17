#!/usr/bin/env bash
# Installs the pinned cargo-deny release binary into the directory given
# as $1 (CI adds it to PATH). The archive is checked against the SHA-256
# GitHub records for the release asset before anything is extracted.
set -euo pipefail

version="0.19.4"
dest="${1:?usage: scripts/install-cargo-deny.sh <install-dir>}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target="x86_64-unknown-linux-musl"
    sha256="3bd58b784e83715b86ddbc9deac591890372ec77fda5741bb0826970b958506f" ;;
  Linux-aarch64) target="aarch64-unknown-linux-musl"
    sha256="c32ded194b38b48e0b44a838a0d85c856120801e1253fc7febf6f2e48ccb84d8" ;;
  Darwin-arm64) target="aarch64-apple-darwin"
    sha256="2437a51d904b29abe2d3aa9ea87e353c86cff108d519ba1e5fb495bb28e7fbaf" ;;
  Darwin-x86_64) target="x86_64-apple-darwin"
    sha256="90fb22528daae11d0ce68cd968ea1f8e4d852b33510d0243d8c6666fc1dbea16" ;;
  *) echo "install-cargo-deny: no pinned build for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

name="cargo-deny-${version}-${target}"
url="https://github.com/EmbarkStudios/cargo-deny/releases/download/${version}/${name}.tar.gz"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
  --output "$work/archive.tar.gz" "$url"
if command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum "$work/archive.tar.gz" | awk '{print $1}')"
else
  got="$(shasum -a 256 "$work/archive.tar.gz" | awk '{print $1}')"
fi
if [ "$got" != "$sha256" ]; then
  echo "install-cargo-deny: checksum mismatch for $name: got $got, want $sha256" >&2
  exit 1
fi
tar -xzf "$work/archive.tar.gz" -C "$work"
mkdir -p "$dest"
install -m 0755 "$work/$name/cargo-deny" "$dest/cargo-deny"
"$dest/cargo-deny" --version

#!/usr/bin/env bash
# Installs the pinned cargo-deny release binary into the directory given
# as $1 (CI adds it to PATH). The archive is checked against the SHA-256
# GitHub records for the release asset before anything is extracted.
# `version` is the one pin: scripts/check.sh fails when the cargo-deny it
# finds is any other release, so the local gate and CI run the same one.
set -euo pipefail

version="0.21.0"
dest="${1:?usage: scripts/install-cargo-deny.sh <install-dir>}"

# 0.21.0 publishes no x86_64-apple-darwin build; build that one with
# `cargo install --locked cargo-deny@${version}`.
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target="x86_64-unknown-linux-musl"
    sha256="a2170b854ff697e79e39c4c538981c9c1bdd803addbae6719a4cb66b4585a903" ;;
  Linux-aarch64) target="aarch64-unknown-linux-musl"
    sha256="47cc5d88d43cd568028080fece02d729f56f0ff8e6c168563ce6589e3c0f4738" ;;
  Darwin-arm64) target="aarch64-apple-darwin"
    sha256="c99a789160775ada8cb54aed72cfdae5a9b72e27434f8868d17c9634c3f52425" ;;
  *) echo "install-cargo-deny: no pinned build for $(uname -s)-$(uname -m);" \
       "use cargo install --locked cargo-deny@${version}" >&2; exit 1 ;;
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

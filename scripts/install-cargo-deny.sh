#!/usr/bin/env bash
# Installs the pinned cargo-deny release binary into the directory given
# as $1 (CI adds it to PATH). The archive is checked against the SHA-256
# GitHub records for the release asset before anything is extracted.
# `version` is the one pin: scripts/check.sh fails when the cargo-deny it
# finds is any other release, so the local gate and CI run the same one.
# A release is pinned only once it is at least two weeks old: 0.20.2 was
# published 2026-07-09. 0.21.0 (2026-11-16) can replace it on or after
# 2026-11-30, as a reviewed change.
set -euo pipefail

version="0.20.2"
dest="${1:?usage: scripts/install-cargo-deny.sh <install-dir>}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target="x86_64-unknown-linux-musl"
    sha256="9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f" ;;
  Linux-aarch64) target="aarch64-unknown-linux-musl"
    sha256="995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d" ;;
  Darwin-arm64) target="aarch64-apple-darwin"
    sha256="fe67d82a10d8597a3549364cb733a3f9cc1bfff9031b7ae46384a9f2a72090c3" ;;
  Darwin-x86_64) target="x86_64-apple-darwin"
    sha256="248da7f581724e470071990c088ffc55c811981715f4cbdb258621fb79f8b7a6" ;;
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

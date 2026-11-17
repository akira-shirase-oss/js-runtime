#!/usr/bin/env bash
# The gate CI runs, and the one to run before every commit: the pinned
# toolchain, formatting, Clippy with warnings as errors, the whole test
# suite against the committed lockfile, and the full cargo-deny check
# (advisories, licences, bans, sources). Each step's exit code is checked
# where it runs; the first failure stops the gate and names the step.
set -uo pipefail
cd "$(dirname "$0")/.."

step() {
  echo "==> $*"
  "$@"
  local status=$?
  if [ "$status" -ne 0 ]; then
    echo "check.sh: FAILED (exit $status): $*" >&2
    exit "$status"
  fi
}

step scripts/check-toolchain.sh
# cargo-deny is part of the gate: a missing binary fails it, like a
# failing check does, and so does a release other than the one CI pins
# (scripts/install-cargo-deny.sh), since checks and config semantics
# change between releases.
pinned="$(sed -n 's/^version="\(.*\)"$/\1/p' scripts/install-cargo-deny.sh)"
if ! found="$(cargo deny --version 2>/dev/null)"; then
  echo "check.sh: FAILED: cargo-deny is not installed" >&2
  echo "fix: scripts/install-cargo-deny.sh <dir> (then put <dir> first on PATH)," \
    "or cargo install --locked cargo-deny@${pinned}" >&2
  exit 1
fi
if [ "${found}" != "cargo-deny ${pinned}" ]; then
  echo "check.sh: FAILED: found ${found}, but the gate pins cargo-deny ${pinned}" >&2
  echo "fix: scripts/install-cargo-deny.sh <dir> (then put <dir> first on PATH)," \
    "or cargo install --locked cargo-deny@${pinned}" >&2
  echo "note: cargo searches ~/.cargo/bin before PATH unless ~/.cargo/bin is on PATH;" \
    "list it after <dir>" >&2
  exit 1
fi
echo "==> cargo-deny ${pinned} (pinned)"
step cargo fmt --all -- --check
step cargo clippy --workspace --all-targets --locked -- -D warnings
step cargo test --workspace --locked
step cargo deny --locked check
echo "check.sh: all checks passed"

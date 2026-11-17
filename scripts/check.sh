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
step cargo fmt --all -- --check
step cargo clippy --workspace --all-targets --locked -- -D warnings
step cargo test --workspace --locked
# cargo-deny is part of the gate: a missing binary fails it, like a
# failing check does.
if ! cargo deny --version >/dev/null 2>&1; then
  echo "check.sh: FAILED: cargo-deny is not installed" >&2
  echo "fix: cargo install --locked cargo-deny, or scripts/install-cargo-deny.sh <dir>" >&2
  exit 1
fi
step cargo deny --locked check
echo "check.sh: all checks passed"

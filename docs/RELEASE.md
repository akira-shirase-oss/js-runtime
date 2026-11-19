# Release checklist — jsrt

How a jsrt version is cut. The maintainer tags; this repo's automation does
not publish anything (no crates.io, no binaries, `publish = false`).

## Before tagging

1. **Evidence current.** The committed evidence was produced on the code
   being released, or the gap is stated in the files themselves:
   - `perf/results/` — load-matrix artifacts name their commit; see
     `perf/LOAD_MATRIX.md` for which are current-scale vs reduced local
     runs (`JSRT_LOAD_UNITS` is recorded in each artifact).
   - `examples/webhook/` — `drill.log` + `runs.jsonl` from a drill run on
     this build (all five phases green).
   - A missed run stays on disk; it is superseded by a named newer
     artifact, not deleted.
2. **Gate green on the release commit.**

   ```sh
   scripts/check.sh
   ```

   Toolchain pin (1.99.1), fmt, clippy `-D warnings`, the full workspace
   test suite, `cargo deny --locked check`. CI runs the same script on
   ubuntu-latest and macos-latest.
3. **Exit-code spot check** on the release binary:

   ```sh
   cargo build --release --locked
   target/release/jsrt run scripts/hello.js   # 0
   target/release/jsrt run missing.js         # 3
   ```

   plus a syntax error → 4 and an uncaught throw → 5.
4. **Version and changelog.** `version` in `Cargo.toml`'s
   `[workspace.package]` equals the release; `Cargo.lock` is regenerated
   (`cargo check`); `CHANGELOG.md` has a dated section for it.
5. **Docs honest.** README's status line, test counts and "Still open"
   list match the code; `perf/LOAD_MATRIX.md` marks every scenario's
   evidence status.

## Tagging (maintainer)

```sh
git tag -a v1.0.0 -m "jsrt v1.0.0"
git push origin v1.0.0
```

Tag the commit that passed step 2 — not a branch head that moved since.

## After tagging

- The release notes source is `CHANGELOG.md`; the evidence appendix is
  `perf/LOAD_MATRIX.md`.
- Full-scale load runs are available on demand:
  `.github/workflows/load.yml` (`workflow_dispatch`, ubuntu-24.04) runs
  `perf/load.py all` and uploads `perf/results/` as artifacts; it never
  pushes back. Committing chosen artifacts is a maintainer action.
- The optional 24 h soak (`perf/soak.py`) strengthens the stability claim
  but is not a release gate — the one-hour result is committed.

## Known limits carried into v1.0.0

Kept honest in README "Still open": non-V8-heap memory is bounded only by
`jsrt supervise --max-rss-mb`; a single call allocating more than ~1 GiB
past the heap limit can still abort V8; unsupervised `jsrt run` output is
uncapped; `.js`/`.mjs` under the entry directory is importable and
enumerable — keep an entry script in a directory of its own.

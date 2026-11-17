# Testing guide: jsrt (`js-runtime`)

https://github.com/akira-shirase-oss/js-runtime

`jsrt` is a purpose-built JavaScript host: V8 runs the script through `deno_core`, and the Rust host owns capabilities, the process lifecycle, and exit codes. Scripts see one namespace, `rt.*`. There is no ambient `Deno`, and with no manifest there is no filesystem and no network — the script can compute and print, nothing else. The M0–M7 roadmap is marked complete in the README.

Written against commit `c30ac2e` (2026-11-07) and updated for the 2026-11-16 security fixes, their review follow-ups and the security verification pass; see "Changes since `c30ac2e`" below and the README's "Breaking changes" and "Security verification". The code is MIT (`LICENSE`).

## Prerequisites

- Rust via [rustup](https://rustup.rs). `rust-toolchain.toml` pins **1.99.1**, which rustup installs on first use; Homebrew's `cargo` ignores the file, and `scripts/check-toolchain.sh` fails when the `rustc` on `PATH` is not 1.99.1. Edition is 2021.
- Optional: [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) for the full gate (`cargo install --locked cargo-deny`, or `scripts/install-cargo-deny.sh <dir>`).
- The first `cargo build` downloads about **95 MB** of prebuilt V8 (`deno_core = "=0.413.0"` → `v8 150.4.0`). `V8_FROM_SOURCE=1` builds V8 yourself (slow, usually unnecessary).
- Optional: `curl` / `nc` for `examples/webhook/drill.sh`.

```sh
# macOS, if rustup isn't first on PATH
export PATH="$(brew --prefix rustup)/bin:$PATH"
```

## Setup and run

```sh
git clone https://github.com/akira-shirase-oss/js-runtime.git
cd js-runtime
cargo build --locked
cargo test --workspace --locked

# or the whole gate CI runs: toolchain, fmt, clippy, tests, cargo-deny
scripts/check.sh

# smoke
target/debug/jsrt run scripts/hello.js
# expected: "hello from jsrt 0.1.0 on v8", "answer: 42", "slept 10ms and returned", exit 0
```

The usual invocation is:

```sh
jsrt run script.js --timeout 5000 --manifest jsrt.manifest.json
```

`--timeout` is milliseconds (default 10000; `0` means no limit). `--max-heap-mb N` caps the V8 heap (default: V8's own ceiling); reaching either ends the run with exit 6. No `--manifest` means deny-all for filesystem, network, kv, media, and serve.

Example manifest shape (from the README):

```json
{
  "read": ["./data"],
  "net": ["api.example.com", "127.0.0.1:3000"],
  "kv":    { "path": "data/app.kv", "max_bytes": 8388608 },
  "media": { "backend": "local", "root": "media/", "max_bytes": 268435456 },
  "serve": { "host": "127.0.0.1", "port": 9400, "max_body": 8388608 }
}
```

CLI flags `--read`, `--net`, `--net-private`, `--kv`, `--media-root`, and `--serve` **extend** the manifest only; they never weaken it.

A small real app (HTTP webhook + embedded SQLite kv, under the supervisor):

```sh
cd examples/webhook
# JSRT defaults to ../../target/debug/jsrt
./drill.sh
```

`jsrt supervise` runs the script in a worker process (optional `--max-rss-mb`, `--max-cpu`, `--wall-ms`, `--report`, `--max-output-kb`).

Exit-code contract (don't collapse these):

| Code | Meaning |
|---|---|
| 0 | ran to completion |
| 2 | bad CLI args |
| 3 | entry not found / couldn't resolve |
| 4 | syntax / compile |
| 5 | uncaught exception |
| 6 | watchdog killed it (timeout), or the V8 heap hit its limit — not a script error |
| 7 | uncaught `CapabilityError` |
| 8 | host failed (bad manifest, internal) |

**What was verified on a Linux box (2026-11-10, America/Toronto):** `cargo test --workspace --locked` passed (57 lifecycle + 6 serve + 8 supervise + 13 host tests). `jsrt run scripts/hello.js` exited 0 with the expected output. A `rt.readText("/etc/passwd")` with no manifest exited **7** with `CapabilityError`. A manifest with `"serve": { "host": "localhost", ... }` exited **8**: `manifest serve host 'localhost' is not an IP literal`.

## Changes since `c30ac2e`

`cargo test --workspace --locked` now runs **140 tests** (75 lifecycle + 6 security + 28 serve + 11 supervise + 20 host), not 84. CI runs `scripts/check.sh` on ubuntu-latest and macos-latest for every push and pull request. New behaviour worth trying:

- `typeof Deno` and `typeof globalThis.__bootstrap` are `"undefined"`; `Deno.core.ops.op_panic(...)` is a `ReferenceError`, not a host abort.
- A granted host that answers `302` to an ungranted one: exit **7**, and the ungranted host sees no connection.
- `--net localhost:PORT` refuses (exit 7, "not a public address"); `--net-private localhost:PORT` or `{"host": "localhost:PORT", "private": true}` allows it. `HTTP(S)_PROXY` is ignored.
- Beside the entry, `import x from './a.json' with { type: 'json' }` (or a text/bytes import, or `import './credentials'`) refuses with exit 7 unless the directory is a declared `read` path.
- `rt.sleep(1e30)` under `--timeout 500` exits 6.
- Fetch errors name only `scheme://host:port`.
- `rt.serve`: a slowloris head is closed after `header_timeout_ms`, a stalled body gets 408, connections past `max_connections` wait, a client that never reads a big answer loses its slot after `write_timeout_ms`, 64 stalled uploads do not delay a GET, and a handler returning `undefined` (or a bad header/status) gets 500 with a short stderr line naming only the first path segment. With the 64 MiB body budget full, a large upload gets `503` with `Retry-After` at once (use `Expect: 100-continue`), while bodies up to 64 KiB are never affected; `server.close()` ends the server even with keep-alive clients connected. A body that declares a length and sends nothing gets 408 after `body_first_byte_ms` (5 s) and frees its budget share; at the connection cap a new client closes the longest-idle connection instead of waiting.
- `jsrt supervise --max-output-kb 16` on a flooding worker: a cap notice while it runs, the stream's last 4 KiB re-shown at exit, and `output.*` counts in the report.
- From the verification pass:
  - `import 'https://example.com/x.js'`, a `data:` URL or a `blob:` URL: exit 7 with `import denied: <scheme>: imports are not supported`, and nothing is fetched, even with a `net` grant for that host.
  - A request head over 16 KiB, or with 65 or more header lines, gets 431 and never reaches the handler; the listener keeps answering.
  - `HTTP_PROXY=http://127.0.0.1:9 jsrt run …` still fetches directly.
  - 1,000 parallel `rt.fetch` calls: never more than 32 in flight.
  - `const a = []; while (true) a.push(new Array(1e5).fill({}));` with `--max-heap-mb 64`: exit 6, "terminated at the V8 heap limit", not a crash.
  - A caught `rt.readText` refusal through a symlink in the grant names the path you passed, not where the link points.
  - At `max_connections` 2, a client trickling a head one byte at a time cannot get a paused download evicted; the trickler is closed instead. An 8 KiB body sent slowly is no longer cut by the rate rule (only `body_timeout_ms` applies to bodies of 64 KiB or less).
  - `jsrt run main.js` from its own directory imports `./dep.js` (it used to refuse with exit 7).

## Happy-path checklist

- [ ] `cargo test --workspace --locked` is green.
- [ ] `jsrt run scripts/hello.js` prints the hello lines and exits 0.
- [ ] A script that only uses `rt.print` / `rt.hostInfo` / `rt.now` works with no manifest.
- [ ] `examples/webhook/drill.sh` deploys, updates, breaks (handler throws → HTTP 500, host still up), inspects the JSONL report, and rolls back with prior kv data still there.

## Try to break it

**Deny-by-default capabilities**

- Call `rt.readText`, `rt.fetch`, `rt.kv.*`, `rt.media.*`, or `rt.serve` with no manifest (or without that grant). An uncaught refusal must exit **7** with `e.name === "CapabilityError"`. Catching the error and continuing should exit 0 (`a_caught_capability_refusal_is_fine`).
- Grant `./data` and try `../` or a symlink that points outside it. Paths are canonicalized; escape should still refuse (`a_read_outside_the_grant_still_refuses`, `a_symlink_inside_the_fence_cannot_point_out`).
- `rt.fetch` to a host that is not in `net`, a `file://` URL, or an authority smuggle, should refuse and must not connect elsewhere.
- `--read` / `--net` added on the CLI should only widen grants, never punch a hole that shrinks a tighter manifest (`cli_flags_extend_never_weaken`).
- `serve` accepts loopback **IP literals only**. `"localhost"` and public addresses should fail at manifest parse (exit 8), not bind. A handler throw should be an HTTP 500, not a dead host (`serve_answers_requests_and_survives_a_handler_crash`).
- `rt.media.put` of a file you cannot `read` should refuse. Over-quota `kv` / `media` writes should refuse and leave no partial object.

**The timeout is real**

- A tight `--timeout` around a `while (true) {}` loop should exit **6** (terminated), not 5.
- A stalled promise / pending timer should also be 6 (`a_stalled_promise_dies_terminated`).
- A V8-heap bomb ends in-process as exit 6 (`--max-heap-mb` lowers the ceiling). Memory outside the V8 heap (large `ArrayBuffer`s) is *not* covered in-process — that is what `jsrt supervise --max-rss-mb ...` is for. Try a supervise run with a tight RSS or CPU budget and confirm the report records the kill.

**Classification**

- Missing file → 3, `syntax error` → 4, `throw new Error` → 5. Don't let a timeout be reported as an exception, or a capability miss as a host crash.

## How to report a bug

Send Vinay (`akira-shirase-oss` / @404humandev) a note, or open a GitHub issue, including:

- Commit SHA (`git rev-parse HEAD`) and `jsrt --version` if it printed one.
- OS, `rustc --version`.
- The exact `jsrt run` / `jsrt supervise` command, the script (or a reduced case), and the manifest.
- The exit code you got vs the table above, plus stderr. For `CapabilityError`, include `e.name` and the message.
- For supervise bugs: the `--report` JSONL line and the flags (`--max-rss-mb`, `--max-cpu`, `--wall-ms`).

## Setup notes we hit

- (fixed 2026-11-16: the repository now has an MIT `LICENSE`.)
- (fixed 2026-11-16: `rust-toolchain.toml` pins Rust 1.99.1, and CI runs the full gate on Linux and macOS.)

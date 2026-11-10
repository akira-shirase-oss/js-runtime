# Testing guide: jsrt (`js-runtime`)

https://github.com/akira-shirase-oss/js-runtime

`jsrt` is a purpose-built JavaScript host: V8 runs the script through `deno_core`, and the Rust host owns capabilities, the process lifecycle, and exit codes. Scripts see one namespace, `rt.*`. There is no ambient `Deno`, and with no manifest there is no filesystem and no network — the script can compute and print, nothing else. The M0–M7 roadmap is marked complete in the README.

Written against commit `c30ac2e` (2026-11-07). `Cargo.toml` says MIT; there is no `LICENSE` file.

## Prerequisites

- A recent Rust toolchain via [rustup](https://rustup.rs). This repo has **no** `rust-toolchain.toml` and does not set `rust-version`. Edition is 2021. It built and tested cleanly on **Rust 1.99.1** in this pass; the README still shows the macOS rustup PATH hint.
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

# smoke
target/debug/jsrt run scripts/hello.js
# expected: "hello from jsrt 0.1.0 on v8", "answer: 42", "slept 10ms and returned", exit 0
```

The usual invocation is:

```sh
jsrt run script.js --timeout 5000 --manifest jsrt.manifest.json
```

`--timeout` is milliseconds (default 10000; `0` means no limit). No `--manifest` means deny-all for filesystem, network, kv, media, and serve.

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

CLI flags `--read`, `--net`, `--kv`, `--media-root`, and `--serve` **extend** the manifest only; they never weaken it.

A small real app (HTTP webhook + embedded SQLite kv, under the supervisor):

```sh
cd examples/webhook
# JSRT defaults to ../../target/debug/jsrt
./drill.sh
```

`jsrt supervise` runs the script in a worker process (optional `--max-rss-mb`, `--max-cpu`, `--wall-ms`, `--report`).

Exit-code contract (don't collapse these):

| Code | Meaning |
|---|---|
| 0 | ran to completion |
| 2 | bad CLI args |
| 3 | entry not found / couldn't resolve |
| 4 | syntax / compile |
| 5 | uncaught exception |
| 6 | watchdog killed it (timeout) — not a script error |
| 7 | uncaught `CapabilityError` |
| 8 | host failed (bad manifest, internal) |

**What was verified on a Linux box (2026-11-10, America/Toronto):** `cargo test --workspace --locked` passed (57 lifecycle + 6 serve + 8 supervise + 13 host tests). `jsrt run scripts/hello.js` exited 0 with the expected output. A `rt.readText("/etc/passwd")` with no manifest exited **7** with `CapabilityError`. A manifest with `"serve": { "host": "localhost", ... }` exited **8**: `manifest serve host 'localhost' is not an IP literal`.

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
- Memory bombs are *not* covered in-process — that is what `jsrt supervise --max-rss-mb ...` is for. Try a supervise run with a tight RSS or CPU budget and confirm the report records the kill.

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

- No `LICENSE` file, despite `license = "MIT"` in `Cargo.toml`.
- No CI and no `rust-toolchain.toml`, so a friend's default `rustc` may not match what this was tested with. Rust 1.99.1 worked.

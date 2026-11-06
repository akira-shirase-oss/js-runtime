# jsrt — a purpose-built JavaScript host

`jsrt` is a JavaScript runtime where the engine is borrowed and everything
that matters is ours. V8 executes JavaScript through `deno_core` (the same
crate Deno builds on); the Rust host owns capabilities, the lifecycle, and
honest errors. The goal is a useful, purpose-built host — not Deno/Node
compatibility and not a new JavaScript engine.

```
jsrt run script.js --timeout 5000 --manifest jsrt.manifest.json
```

## What scripts see

Exactly one namespace — `rt.*`. No ambient `Deno`, no ungated global:

| API | What it does | Gate |
|---|---|---|
| `rt.print(msg)` | stdout, no added newline | none |
| `rt.hostInfo()` | `{runtime, version, engine}` | none |
| `rt.now()` | wall-clock ms for measurements | none |
| `rt.sleep(ms)` | async timer | deadline (run timeout) |
| `rt.readText(path)` | read a text file | declared `read` paths only |
| `rt.fetch(url)` | HTTP GET → `{status, statusText, body, bytes, durationMs}` | declared `net` destinations, 10 s deadline, 4 MiB body cap |

`console.log` also works (deno_core's minimal console). Anything not listed
does not exist for the script — that is the point.

## The manifest (`jsrt.manifest.json`)

```json
{ "read": ["./data"], "net": ["api.example.com", "127.0.0.1:3000"] }
```

- Deny by default: no manifest, no filesystem and no network — the script can
  compute and print, nothing else.
- Paths are canonicalized before checking (symlinks and `..` cannot escape a
  declared directory); `host` alone allows any port, `host:port` is exact.
- `--read`/`--net` CLI flags *extend* the manifest — additive only, never
  weakening.
- Refusals throw a `CapabilityError` in JS — scripts can catch and match on
  `e.name`.

## How a run can end — the exit-code contract

| Code | Class | Meaning |
|---|---|---|
| 0 | ok | ran to completion |
| 2 | usage | bad CLI args (clap) |
| 3 | load | entry not found / couldn't resolve |
| 4 | syntax | parsed but failed to compile |
| 5 | exception | ran and threw uncaught |
| 6 | terminated | the watchdog killed it (timeout) — *not* a script error |
| 7 | capability | an uncaught `CapabilityError` escaped the script |
| 8 | host | the host itself failed (manifest unreadable, internal) |

A terminated run is never confused with a thrown run — those are different
kinds of failure and scripts/deploys need to tell them apart.

## The timeout is real

`tokio::time::timeout` around the event loop cannot preempt a CPU-bound
script — the executor thread is blocked inside the poll. The watchdog is a
separate thread that calls V8's `IsolateHandle::terminate_execution()` on the
deadline, which interrupts running JS *and* a stalled event loop. A tokio
backstop covers the fully-idle case. **What this does not cover:** memory
bombs — a runaway allocation needs a process boundary (the supervisor
milestone, M4), and we don't pretend an in-process flag replaces it.

## Layout

```
crates/jsrt        the CLI: args → HostOptions → exit code
crates/jsrt-host   the host: JsRuntime, ops, capabilities, watchdog, taxonomy
  js/bootstrap.js  the rt.* namespace — the API contract, kept tiny on purpose
scripts/           example scripts
perf/              measurement harness (cold start, op boundary, load)
docs/              ROADMAP and module notes
```

## Dependencies — one tested set, pinned

- `deno_core = "=0.413.0"` → pins `v8 150.4.0` (prebuilt static lib from
  denoland's GitHub releases; `V8_FROM_SOURCE=1` to build V8 itself).
- `tokio` current-thread executor — `JsRuntime` is `!Send`; every run stays
  on the thread that created it.
- `reqwest` (rustls/webpki-roots) behind `rt.fetch`.
- Extension JS must be 7-bit ASCII — deno_core asserts it.

Build: `export PATH="$(brew --prefix rustup)/bin:$PATH"; cargo build` —
the first build downloads ~95 MB of prebuilt V8.

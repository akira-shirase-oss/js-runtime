# jsrt — a purpose-built JavaScript host

`jsrt` is a JavaScript runtime where the engine is borrowed and everything
that matters is ours. V8 executes JavaScript through `deno_core` (the same
crate Deno builds on); the Rust host owns capabilities, the lifecycle, and
honest errors. The goal is a useful, purpose-built host — not Deno/Node
compatibility and not a new JavaScript engine.

```
jsrt run script.js --timeout 5000 --manifest jsrt.manifest.json
```

**Status: the M0–M7 roadmap is complete.** Deny-by-default capabilities, the
supervisor, the stdlib wave (`rt.kv`, `rt.media`, `rt.serve`) and the
million-scale evidence (1M op calls, 1M requests through `rt.serve`, 1,000 cold
starts, a one-hour soak) are all built, with committed results under
`perf/results/`.

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
| `rt.setTimeout` `setInterval` `clearTimeout` `clearInterval` | Node-style timers, id-tracked; `unrefTimer`/`refTimer` release the loop | deadline (run timeout) |
| `rt.pendingTimers()` | how many timers this script still owns | none |
| `rt.kv.get/put/delete/list` | embedded SQLite store; `list` is an exact-prefix scan | declared `kv` file + `max_bytes` quota |
| `rt.media.put/stat` | streamed content-addressed uploads (sha256 names) | declared `media` root; the source still needs a `read` grant |
| `rt.serve(opts, handler)` | HTTP listener, pull-model dispatch; a handler crash is a 500, not a dead host | declared `serve` host/port — loopback IP literals only — plus `max_body` |

`console.log` also works (deno_core's minimal console). Anything not listed
does not exist for the script — that is the point.

## The manifest (`jsrt.manifest.json`)

```json
{
  "read": ["./data"],
  "net": ["api.example.com", "127.0.0.1:3000"],
  "kv":    { "path": "data/app.kv", "max_bytes": 8388608 },
  "media": { "backend": "local", "root": "media/", "max_bytes": 268435456 },
  "serve": { "host": "127.0.0.1", "port": 9400, "max_body": 8388608 }
}
```

- Deny by default: no manifest, no filesystem and no network — the script can
  compute and print, nothing else.
- Paths are canonicalized before checking (symlinks and `..` cannot escape a
  declared directory); `host` alone allows any port, `host:port` is exact.
- `serve` binds loopback IP literals only — `"localhost"` and public addresses
  are refused at manifest parse.
- `--read`/`--net`/`--kv`/`--serve` CLI flags *extend* the manifest — additive
  only, never weakening.
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
bombs — a runaway allocation needs a process boundary, which is what
`jsrt supervise` is for (`RLIMIT_CPU` seconds, RSS polling, SIGTERM→SIGKILL,
bounded JSONL run reports), and we don't pretend an in-process flag replaces
it.

## Layout

```
crates/jsrt        the CLI: args → HostOptions → exit code (`run`, `supervise`)
crates/jsrt-host   the host: JsRuntime, ops, capabilities, watchdog, taxonomy
  js/bootstrap.js  the rt.* namespace — the API contract, kept tiny on purpose
crates/jsrt-load   the HTTP load driver behind the perf evidence
examples/webhook   a real app on rt.serve + rt.kv, run under `jsrt supervise`
scripts/           example scripts
perf/              measurement harness (cold start, op boundary, load, soak)
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

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

## Breaking changes (2026-11-16 security fixes)

Scripts and deployments written against earlier builds can behave
differently:

- **No `Deno` globals.** `Deno`, `Deno.core` and `__bootstrap` no longer
  exist for scripts (they reached every builtin op, including `op_panic`);
  the unused `op_noop` is gone.
- **Name grants reach public addresses only.** A `localhost`, Docker-service
  or other privately-resolving name grant now refuses (exit 7) unless it is
  opted in with `{"host": ..., "private": true}` or `--net-private`.
- **Environment proxies are ignored** by `rt.fetch` (see the manifest notes).
- **Redirects are re-checked.** A redirect to an ungranted destination is a
  `CapabilityError` (exit 7) instead of being followed.
- **The entry directory admits `.js`/`.mjs` modules only.** A `./config.json`
  or text import beside the entry, and `.cjs`, `.ts` or extensionless
  modules, now need a declared `read` path (exit 7 otherwise).
- **`rt.serve` has limits and stricter answers.** Defaults: 256 connections
  (the rest wait in the accept backlog), 10 s to send a head and between
  keep-alive requests, 30 s for a body (then 408), 30 s without write
  progress before a connection is dropped, 413 for a declared
  `Content-Length` over `max_body` before anything is read. A status outside
  200-599, a bad header or a handler that returns no object is a 500 with a
  stderr line.
- **`jsrt supervise` caps worker output** at 8 MiB per stream per run
  (`--max-output-kb`); the run report gains an `output` object.

## What scripts see

Exactly one namespace — `rt.*`. No ambient `Deno`, no ungated global:
deno_core installs `globalThis.Deno.core` (the raw ops table, including
`op_panic`) and `globalThis.__bootstrap` for extension code; the bootstrap
captures what it needs and deletes both before any script runs, and scripts
cannot import `ext:` modules. A test proves `op_panic` is unreachable.

| API | What it does | Gate |
|---|---|---|
| `rt.print(msg)` | stdout, no added newline | none |
| `rt.hostInfo()` | `{runtime, version, engine}` | none |
| `rt.now()` | wall-clock ms for measurements | none |
| `rt.sleep(ms)` | async timer | deadline (run timeout) |
| `rt.readText(path)` | read a text file | declared `read` paths only |
| `rt.fetch(url)` | HTTP GET → `{status, statusText, body, bytes, durationMs}` | declared `net` destinations — re-checked on every redirect hop (at most 10), never through an environment proxy — one 10 s deadline over the whole exchange (connect, redirects, headers, every body chunk), 4 MiB body cap; errors name only `scheme://host:port`, never the path, query or credentials |
| `rt.setTimeout` `setInterval` `clearTimeout` `clearInterval` | Node-style timers, id-tracked; `unrefTimer`/`refTimer` release the loop | deadline (run timeout) |
| `rt.pendingTimers()` | how many timers this script still owns | none |
| `rt.kv.get/put/delete/list` | embedded SQLite store; `list` is an exact-prefix scan | declared `kv` file + `max_bytes` quota |
| `rt.media.put/stat` | streamed content-addressed uploads (sha256 names) | declared `media` root; the source still needs a `read` grant |
| `rt.serve(opts, handler)` | HTTP listener, pull-model dispatch; a handler that throws, returns no object, or returns an invalid status (outside 200-599) or header (a bad name, or a value that is an object, function or nested array; an array value repeats the header, as `set-cookie` needs) is a 500 plus a stderr line (path without its query, length-capped, at most 10 lines a minute with the rest counted) — not a dead host, an empty 200 or a hung client | declared `serve` host/port — loopback IP literals only — plus `max_body`, `max_connections` (default 256, at most 65,536; the rest wait in the accept backlog), `header_timeout_ms` (default 10 s, also bounds keep-alive idling) and `body_timeout_ms` (default 30 s, then 408) and `write_timeout_ms` (default 30 s: a response write with no progress for that long — a client that stopped reading — drops the connection and frees its slot); bodies are read before they queue for the script and share a 64 MiB per-listener buffer budget |

`console.log` also works (deno_core's minimal console). Anything not listed
does not exist for the script — that is the point.

## The manifest (`jsrt.manifest.json`)

```json
{
  "read": ["./data"],
  "net": ["api.example.com", "127.0.0.1:3000", {"host": "localhost:5432", "private": true}],
  "kv":    { "path": "data/app.kv", "max_bytes": 8388608 },
  "media": { "backend": "local", "root": "media/", "max_bytes": 268435456 },
  "serve": { "host": "127.0.0.1", "port": 9400, "max_body": 8388608 }
}
```

- Deny by default: no manifest, no filesystem and no network — the script can
  compute and print, nothing else.
- Imports: the entry's directory, and everything below it, is program
  space for `.js`/`.mjs` modules — a script may import any of them and read
  their exports, and their identifiers can surface in error messages. Text,
  bytes and JSON imports (`with { type: ... }`) and any other file imported
  as a module need a declared `read` path and stay under `rt.readText`'s
  4 MiB cap. So `jsrt run ~/x.js` no longer makes every file under `~`
  importable as text, but it still exposes every `.js`/`.mjs` file under
  `~`: keep an entry script in a directory of its own.
- Paths are canonicalized before checking (symlinks and `..` cannot escape a
  declared directory); `host` alone allows any port, `host:port` is exact.
- A *name* grant (`api.example.com`, `localhost`) connects to public
  addresses only: a DNS answer in loopback, private, link-local (cloud
  metadata), CGNAT or other non-routable ranges is refused with a
  `CapabilityError`. To reach a private host by name — `localhost` in
  development, a Docker or compose service, an internal HTTPS host whose
  certificate names it — opt that name in explicitly:
  `{"host": "db:5432", "private": true}` in the manifest, or
  `--net-private db:5432`. An IP-literal grant (`10.0.0.5:8080`,
  `127.0.0.1:3000`) never goes through DNS and needs no opt-in. A misspelt
  key in the object form is a manifest error, not a silent public grant.
- `rt.fetch` ignores `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` on purpose:
  a proxy would connect, and resolve names, on the script's behalf to
  destinations the host never checked. There is no proxy setting yet, so on
  a proxy-only network fetch cannot reach outside hosts.
- `serve` binds loopback IP literals only — `"localhost"` and public addresses
  are refused at manifest parse.
- `--read`/`--net`/`--net-private`/`--kv`/`--serve` CLI flags *extend* the
  manifest — additive only, never weakening.
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
bounded JSONL run reports, worker stdout and stderr each capped per run by
`--max-output-kb` — default 8 MiB, a lifetime budget per run — with a notice the moment a stream hits it, its last 4 KiB re-shown at exit so the cause of death survives, and passed/dropped/re-shown bytes in the report), and we don't pretend an in-process flag replaces
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

## License

MIT — see [LICENSE](LICENSE).

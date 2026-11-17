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
  (at the cap the longest-idle connection is closed to admit a new one), 10 s to send a head and between
  keep-alive requests, 30 s for a body (then 408), 30 s without write
  progress before a connection is dropped, 413 for a declared
  `Content-Length` over `max_body` before anything is read. A status outside
  200-599, a bad header or a handler that returns no object is a 500 with a
  stderr line. Large uploads can get `503` with `Retry-After` while the
  per-listener body budget is full.
- **`jsrt supervise` caps worker output** at 8 MiB per stream per run
  (`--max-output-kb`); the run report gains an `output` object.

Later the same day, the security verification pass (see "Security
verification" below) changed these:

- **Only local files are modules.** `http:`, `https:`, `data:`, `blob:`
  and every other non-`file:` import is refused by jsrt's own loader as a
  `CapabilityError` (exit 7); it used to fail later inside deno_core with
  exit 3 (static) or 5 (dynamic).
- **`rt.fetch` runs at most 32 exchanges at once** per run. Later calls
  wait for a slot, first come first served, and their 10 s deadline starts
  when they get one.
- **The V8 heap has a limit that ends the run.** Reaching it (V8's default,
  or `--max-heap-mb N`) is exit 6 with "terminated at the V8 heap limit",
  instead of a V8 out-of-memory abort (a signal, no exit code), for the
  cases the tests cover (see "The timeout is real" for the limits of that).
- **Refusal messages name what the script asked for**: the path it passed
  or the import specifier it wrote, never the canonical host path or where
  a symlink points.
- **`rt.serve` eviction at the connection cap** closes the connection that
  has been longest in its current phase (waiting for a head, or reading a
  body). Bodies of 64 KiB or less are bounded by `body_timeout_ms` only, not
  the minimum-rate rule.
- **`jsrt run main.js`** (a bare relative entry) admits its sibling
  `.js`/`.mjs` modules again; it refused them with exit 7.

Then the review of that pass (also 2026-11-16) changed these:

- **A response being written is protected only while its reader keeps the
  rate.** At the connection cap, a response whose client reads below
  `body_min_rate` (after `body_first_byte_ms`) can now be closed for a
  newcomer; before, a response being written was never evicted, so slow
  readers could hold every slot. New optional `response_timeout_ms`.
- **`--max-heap-mb` is 0 or at least 16** (exit 2 otherwise); V8 aborted
  below about 8 MiB even for hello-world.
- **A path outside the read fence is refused before the filesystem is
  asked.** `rt.readText` and `rt.media.put` give one message for every
  refusal but the size cap: "read denied: X is missing or outside every
  declared read path" (it was "does not resolve (…)" or "resolves outside
  …"). Imports say "import denied: X is outside every declared read
  path", and a missing import outside the fence is now exit 7 like an
  existing one (it was 3; a missing one inside the fence is still 3). `..`
  in a `readText` path is folded before symlinks are followed, as in a URL.
- **Only regular files are read.** `rt.readText`, `rt.media.put` and
  imports refuse a FIFO, a device or a directory ("is not a regular file",
  exit 7; a directory used to be exit 8), and `readText` enforces its
  4 MiB cap on the bytes it reads, not only on the size the metadata gave.
- **A manifest key jsrt does not know is an error** (exit 8) naming it:
  `max_byte` used to be ignored, leaving the 64 MiB default.
- **A heap bomb in a callback ends the run.** One inside a timer, a
  microtask, code after an `await` or an `rt.serve` handler is now exit 6
  at once, with or without `--timeout`; it used to leave the process parked
  until the run timeout (forever with `--timeout 0`) or, for a handler,
  exit 0. Under `jsrt run`, a run stopped at the heap limit that has not
  ended 2 s later exits 6 regardless of `--timeout`.
- **Errors no longer show host details**: a kv store that cannot open says
  "kv open: the store cannot be opened (CannotOpen)" (the full error, with
  its path, goes to stderr), and a name that resolves only to private
  addresses no longer lists them in `e.message` (stderr does).

## What scripts see

Exactly one namespace — `rt.*`. No ambient `Deno`, no ungated global:
deno_core installs `globalThis.Deno.core` (the raw ops table, including
`op_panic`) and `globalThis.__bootstrap` for extension code; the bootstrap
captures what it needs and deletes both before any script runs, and scripts
cannot import `ext:` modules. Tests probe the known paths (`Deno.core.ops`,
`__bootstrap.core.ops`, `ext:core/*` imports, the global names); they cannot
prove no other path exists. The deletion relies on deno_core not reading
`globalThis.Deno` after start-up, which holds for 0.413 and must be checked
again on every deno_core upgrade.

| API | What it does | Gate |
|---|---|---|
| `rt.print(msg)` | stdout, no added newline | none |
| `rt.hostInfo()` | `{runtime, version, engine}` | none |
| `rt.now()` | wall-clock ms for measurements | none |
| `rt.sleep(ms)` | async timer | deadline (run timeout) |
| `rt.readText(path)` | read a text file | declared `read` paths only; regular files up to 4 MiB, the cap enforced on the bytes read |
| `rt.fetch(url)` | HTTP GET → `{status, statusText, body, bytes, durationMs}` | declared `net` destinations — re-checked on every redirect hop (at most 10), never through an environment proxy — one 10 s deadline over the whole exchange (connect, redirects, headers, every body chunk), 4 MiB body cap, at most 32 exchanges in flight per run (later calls wait for a slot; the deadline starts when they get one, so bodies being read stay under 128 MiB); errors name only `scheme://host:port`, never the path, query or credentials |
| `rt.setTimeout` `setInterval` `clearTimeout` `clearInterval` | Node-style timers, id-tracked; `unrefTimer`/`refTimer` release the loop | deadline (run timeout) |
| `rt.pendingTimers()` | how many timers this script still owns | none |
| `rt.kv.get/put/delete/list` | embedded SQLite store; `list` is an exact-prefix scan | declared `kv` file + `max_bytes` quota |
| `rt.media.put/stat` | streamed content-addressed uploads (sha256 names) | declared `media` root; the source still needs a `read` grant |
| `rt.serve(opts, handler)` | HTTP listener, pull-model dispatch; `server.close()` stops accepting and ends `server.done` even while keep-alive clients stay connected (their unanswered requests get 503); a handler that throws, returns no object, or returns an invalid status (outside 200-599) or header (a bad name, or a value that is an object, function or nested array; an array value repeats the header, as `set-cookie` needs) is a 500 plus a stderr line (only the first path segment, never the query or deeper segments, length-capped, at most 10 lines a minute with the rest counted) — not a dead host, an empty 200 or a hung client | declared `serve` host/port — loopback IP literals only — plus `max_body`, `max_connections` (default 256, at most 65,536; at the cap a new client makes the server close the connection that has been longest in its current phase — waiting for a head (idle, between keep-alive requests, or a slow head), reading a body, or writing a response to a client that reads too slowly — but never one with a request in the script; a trickled head ages like an idle one, so sending a byte now and then does not protect a connection. A response being written is protected for `body_first_byte_ms` from its start, and each byte the client takes extends that by 1/`body_min_rate` of a second, up to `body_first_byte_ms` ahead: a reader may pause that long and must average `body_min_rate` (64 KiB/s by default) to keep its slot at the cap. The cost: at the cap a newcomer behind slow readers waits up to about `body_first_byte_ms` (5 s by default; measured 3.1 s for one that arrived 2 s after two 1 KiB/s readers), and a legitimate download slower than `body_min_rate` can be closed for a newcomer. While every connection is protected a new client waits in the accept backlog, and the server looks again every 100 ms). A request head over 16 KiB, or with more than 64 header lines, is answered 431 before the script sees it, `header_timeout_ms` (default 10 s, also bounds keep-alive idling) and `body_timeout_ms` (default 30 s, then 408) and `write_timeout_ms` (default 30 s: a response write with no progress for that long — a client that stopped reading — drops the connection and frees its slot; keep it at 5 s or more, since shorter values were seen to cut legitimate slow downloads on a loaded machine) and `response_timeout_ms` (off by default: when set, a response still being written after that long drops the connection, whatever its rate); `max_body` at most 1 GiB; an unknown key in the `serve` object is a manifest error. Bodies are read before they queue for the script. The first 64 KiB of every body is outside the body budget, so small requests never wait on it; a larger declared `Content-Length` reserves its whole length from a 64 MiB per-listener budget before anything is read, and a chunked body takes budget per chunk past 64 KiB. When the budget is full such a request is answered 503 with `Retry-After: 1` at once — never queued, never left to time out (send large uploads with `Expect: 100-continue` to get that answer before sending the body; a client without it has the rest of its body read and discarded for up to 200 ms first, so it can usually finish writing and read the 503). A reservation has to be earned: from when a body first holds budget it gets `body_first_byte_ms` (default 5 s) to send a byte and must then average `body_min_rate` bytes a second (default 65536), or it is answered 408 and its share is released; a body that holds no budget (64 KiB or less) is bounded by `body_timeout_ms` alone; the buffer grows as bytes arrive, never to the declared size up front. What an attacker can still hold: about budget ÷ `max_body` connections (8 at the defaults) that declare `max_body` and send nothing deny other uploads over 64 KiB for 5 s at a time, and can reconnect to repeat it; connections that keep sending at the minimum rate hold their share for up to `body_timeout_ms` (30 s) at 64 KiB/s each. Bodies of 64 KiB or less are never affected. Concurrent large uploads without `Expect: 100-continue` mostly see a broken pipe rather than the 503 (24 at once in a review run: 10 answered 200, 2 read their 503, 12 broke), so large uploads should send it |

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

- Deny by default: no manifest, no network and no filesystem beyond
  importing `.js`/`.mjs` modules from the entry's own directory — the
  script can compute and print, nothing else. A path outside the grants is
  refused before the filesystem is asked about it, with the same message
  whether or not anything is there, so a script cannot probe which host
  paths exist (it can still tell which files exist under its own
  directory, where it may import).
- Imports come from local files only: `http:`, `https:`, `data:`, `blob:`
  and every other non-`file:` specifier is refused by jsrt's loader (exit
  7) before anything is fetched. A `net` grant is for `rt.fetch`, not a
  source of code.
- Imports: the entry's directory, and everything below it, is program
  space for `.js`/`.mjs` modules — a script may import any of them and read
  their exports, and their identifiers can surface in error messages. Text,
  bytes and JSON imports (`with { type: ... }`) and any other file imported
  as a module need a declared `read` path and stay under `rt.readText`'s
  4 MiB cap. So `jsrt run ~/x.js` no longer makes every file under `~`
  importable as text, but it still exposes every `.js`/`.mjs` file under
  `~`: keep an entry script in a directory of its own.
- A path is first normalized by its spelling (made absolute against the
  working directory, `.` and `..` folded) and compared with the declared
  directories, as declared and as canonicalized (`/tmp/x` and
  `/private/tmp/x` both work for a `/tmp/x` grant); only then is it
  canonicalized, so symlinks and `..` cannot escape a declared directory.
  Only regular files are read. `host` alone allows any port, `host:port`
  is exact.
- An unknown key anywhere in the manifest (a misspelt `max_bytes`) is an
  error naming it, exit 8, not a silent default.
- A *name* grant (`api.example.com`, `localhost`) connects to public
  addresses only: a DNS answer in loopback, private, link-local (cloud
  metadata), CGNAT or other non-routable ranges is refused with a
  `CapabilityError`. To reach a private host by name — `localhost` in
  development, a Docker or compose service, an internal HTTPS host whose
  certificate names it — opt that name in explicitly:
  `{"host": "db:5432", "private": true}` in the manifest, or
  `--net-private db:5432`. An IP-literal grant (`10.0.0.5:8080`,
  `127.0.0.1:3000`) never goes through DNS and needs no opt-in. A misspelt
  key in the object form is a manifest error naming the key, not a silent
  public grant. The opt-in is per host name, not per grant: once any grant
  for `db` says `"private": true`, every grant for `db` may resolve to a
  private address (ports are still checked grant by grant).
- `rt.fetch` ignores `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` on purpose:
  a proxy would connect, and resolve names, on the script's behalf to
  destinations the host never checked. There is no proxy setting yet, so on
  a proxy-only network fetch cannot reach outside hosts.
- `serve` binds loopback IP literals only — `"localhost"` and public addresses
  are refused at manifest parse.
- `--read`/`--net`/`--net-private`/`--kv`/`--serve` CLI flags *extend* the
  manifest — additive only, never weakening.
- Refusals throw a `CapabilityError` in JS — scripts can catch and match on
  `e.name`. Its message names what the script asked for (its path or
  specifier), never the canonical host path or a symlink's target.

## How a run can end — the exit-code contract

| Code | Class | Meaning |
|---|---|---|
| 0 | ok | ran to completion |
| 2 | usage | bad CLI args (clap) |
| 3 | load | entry not found / couldn't resolve |
| 4 | syntax | parsed but failed to compile |
| 5 | exception | ran and threw uncaught |
| 6 | terminated | the watchdog killed it (timeout), or the V8 heap reached its limit — *not* a script error |
| 7 | capability | an uncaught `CapabilityError` escaped the script |
| 8 | host | the host itself failed (manifest unreadable, internal) |

A terminated run is never confused with a thrown run — those are different
kinds of failure and scripts/deploys need to tell them apart.

## The timeout is real

`tokio::time::timeout` around the event loop cannot preempt a CPU-bound
script — the executor thread is blocked inside the poll. The watchdog is a
separate thread that calls V8's `IsolateHandle::terminate_execution()` on the
deadline, which interrupts running JS *and* a stalled event loop. A tokio
backstop covers the fully-idle case. The V8 heap gets the same treatment:
a near-heap-limit callback terminates the isolate at V8's default heap
ceiling, or at `--max-heap-mb N` (0 or at least 16), so a heap bomb ends as
exit 6 rather than a V8 abort. The termination lands when the running call
returns, so the first time the ceiling is reached V8 gets 1 GiB more for
that call to finish: the heap can overshoot the ceiling by up to that much
for a moment (peak RSS 0.73 GB for a 50-million-slot `fill` under
`--max-heap-mb 64`, which ends as exit 6). A stopped isolate ends the run
even where deno_core's event loop would not: after V8 terminates a
callback, deno_core 0.413 counts it as no work, so the host stops waiting
on the loop as soon as it stops the isolate, and under `jsrt run` exits 6
if the run has not ended 2 s later, with or without `--timeout`. What the
tests prove: a loop of ordinary allocations and that one large `fill`, at
16 and 64 MiB, and the loop inside a `setTimeout`, a `queueMicrotask`, a
`.then`, code after an `await` and an `rt.serve` handler, all with
`--timeout 0` (the server exits and its port closes). A single
call that allocates more than 1 GiB past the ceiling before it returns can
still make V8 abort (a signal, no exit code; `jsrt supervise` reports it
as one). **What this does not cover:** memory outside the V8 heap —
`ArrayBuffer` backing stores and host buffers — which needs a process
boundary, which is what `jsrt supervise` is for (`RLIMIT_CPU` seconds, RSS polling, SIGTERM→SIGKILL,
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
scripts/           check.sh (the gate CI runs), check-toolchain.sh,
                   install-cargo-deny.sh, and example scripts
.github/           CI on ubuntu-latest and macos-latest, Dependabot
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
the first build downloads ~95 MB of prebuilt V8. `rust-toolchain.toml` pins
Rust 1.99.1 (rustup honours it; Homebrew's cargo does not, and
`scripts/check-toolchain.sh` says so).

Gate: `scripts/check.sh` runs the toolchain check, checks that cargo-deny
is the release CI pins (0.20.2, in `scripts/install-cargo-deny.sh`; a pin
moves only to a release at least two weeks old), then `cargo fmt --check`,
Clippy with `-D warnings`, `cargo test --workspace --locked` (156 tests:
76 lifecycle, 14 security, 31 serve, 11 supervise, 24 host) and the full `cargo deny --locked check` (advisories, licences,
bans, sources). CI runs the same script on ubuntu-latest and macos-latest
on every push and pull request, and daily for new advisories.

## Security verification (2026-11-16)

Each defence below has a regression test that fails when the defence is
removed. Where the defence already existed, the guard was weakened locally
to watch the test fail, then restored.

| Defence | Test |
|---|---|
| Remote, `data:` and `blob:` imports are refused by jsrt's loader, with zero connections | `crates/jsrt/tests/security.rs`: `static_remote_and_data_imports_are_refused_by_jsrts_loader`, `dynamic_remote_data_and_blob_imports_are_refused_by_jsrts_loader` |
| `rt.serve` refuses a head over 16 KiB or more than 64 header lines (431) and keeps serving | `crates/jsrt/tests/serve.rs`: `an_oversized_head_or_too_many_headers_is_refused_and_the_listener_keeps_serving` |
| `rt.fetch` ignores `HTTP(S)_PROXY` and `ALL_PROXY` | `security.rs`: `fetch_ignores_proxy_environment_variables` |
| At most 32 fetches in flight per run | `security.rs`: `a_thousand_parallel_fetches_keep_in_flight_under_the_cap` |
| A heap bomb under `jsrt run` ends as exit 6, not a V8 abort | `security.rs`: `an_allocation_bomb_under_jsrt_run_ends_classified_at_the_heap_limit` |
| One call that allocates about 700 MB past a 64 MiB limit still ends as exit 6 | `security.rs`: `one_call_that_allocates_far_past_the_heap_limit_still_ends_classified` |
| `--max-heap-mb` below 16 is a usage error; at 16 a script runs and a bomb is exit 6 | `security.rs`: `a_heap_limit_below_the_floor_is_refused_and_the_floor_works` |
| A heap bomb in a timer, microtask, `.then` or after an `await` ends as exit 6 with `--timeout 0` | `security.rs`: `a_heap_bomb_inside_a_callback_ends_the_run_even_without_a_timeout` |
| A heap bomb in an `rt.serve` handler ends the server (exit 6) and closes its port, with `--timeout 0` | `serve.rs`: `a_heap_bomb_in_a_handler_ends_the_server_and_closes_the_listener` |
| Caught refusals name no canonical path or symlink target | `security.rs`: `caught_refusals_do_not_reveal_canonical_paths_or_symlink_targets` |
| With no read grant, `readText`, `media.put`, `import.meta.resolve` and `import()` answer alike for an existing and a missing host path | `security.rs`: `a_script_without_a_read_grant_cannot_tell_which_host_paths_exist`; `lifecycle.rs`: `a_missing_import_outside_the_fence_is_refused_like_an_existing_one` |
| A kv store that cannot open does not name its path | `security.rs`: `a_kv_store_that_cannot_open_does_not_name_its_path` |
| `readText` refuses a FIFO instead of reading past its 4 MiB cap | `security.rs`: `read_text_refuses_a_fifo_instead_of_reading_past_its_cap` |
| A refused private name does not show the script its addresses | `security.rs`: `a_refused_private_name_does_not_show_the_script_its_addresses` |
| A misspelt manifest key is an error naming it | `security.rs`: `a_misspelt_manifest_key_is_an_error_naming_it` |
| At the connection cap a trickler cannot get a download evicted mid-write | `serve.rs`: `at_the_cap_a_trickler_cannot_steer_eviction_onto_a_download_being_written` |
| At the connection cap slow readers cannot hold every slot | `serve.rs`: `at_the_cap_slow_readers_cannot_pin_every_slot` |
| `response_timeout_ms` ends a slow download even inside the rate grace | `serve.rs`: `a_response_timeout_ends_a_slow_download_even_below_the_rate_grace` |
| A small slow body gets `body_timeout_ms`; a large one still has to keep the rate | `serve.rs`: `a_slow_small_body_gets_the_body_timeout_not_the_rate_deadline` |
| `unsafe` only in the supervisor, each block documented | `#![forbid(unsafe_code)]` in jsrt-host and jsrt-load; Clippy `undocumented_unsafe_blocks = "deny"` in the gate |
| Supply chain: advisories, licences, bans, sources | `cargo deny --locked check` in the gate, the same pinned cargo-deny release locally and in CI |

Still open: memory outside the V8 heap is bounded only by `jsrt supervise
--max-rss-mb`; one call that allocates more than 1 GiB past the heap limit
can still abort V8; output under unsupervised `jsrt run` is not capped; any
`.js`/`.mjs` file under the entry directory is importable, and a script can
tell which files exist there (keep an entry in a directory of its own);
`import.meta.url` and a failed import of the script's own modules show its
directory's canonical path, as ESM does; at the `rt.serve` connection cap
a newcomer behind slow readers can wait up to `body_first_byte_ms`; a
data import is checked against the 4 MiB cap by its metadata and then read
whole by deno_core's loader, so a file that grows in between is read whole;
the M7 load runs were repeated on the current build on 2026-11-17 (`f83f39d`, all passed; see ROADMAP), and the webhook drill is still owed a re-run.

## License

MIT — see [LICENSE](LICENSE).

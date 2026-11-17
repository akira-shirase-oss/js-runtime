# ROADMAP — jsrt

Each step states the outcome, the invariant, the failure case and the
acceptance test. Nothing is called done without its evidence.

## M0 — scaffold (done)

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 0.1 workspace | `jsrt` CLI + `jsrt-host` lib, `deno_core =0.413.0`, `rt.*` namespace, error taxonomy, watchdog | engine borrowed, host owned; every ending has a distinct exit code | syntax vs exception vs load vs terminated must never blur | hello.js runs (0), missing (3), syntax (4), throw (5) — verified live |

## M1 — execute (done)

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 1.1 run lifecycle | integration suite pinning the taxonomy end to end | every exit class reproducible | uncaught `CapabilityError` → 7; watchdog kill → 6 | `CARGO_BIN_EXE_jsrt` tests assert each code — 15 tests, `358677b` |
| 1.2 module semantics | real `import` between files works; load errors name the failing specifier | module graph is real, not eval-in-a-trenchcoat | circular import and missing import classified honestly | 5 lifecycle tests + host-level dep-syntax test, `8294ff8` |
| 1.3 op boundary measured | perf harness: cold-start p50/p95 over N runs, op-call cost over 10k calls | numbers recorded with commit + machine, never presented as production claims | a regression is evidence, not a rerun reason | `perf/results/m1-3-*-8294ff8.json` |

M1 findings kept honest in the artifact: the watchdog needed a second
stage (a parked event loop has no JS to interrupt — `terminate_grace`
escalates to process exit, the in-process stand-in for M4's kill), and
the timer floor is ~1.2ms (`sleep(1)` lands ~2.3ms) — sub-ms timing is
not a thing scripts can rely on.

## M2 — async host (done)

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 2.1 timers | `setTimeout`/`clearTimeout`/`setInterval`/`unref` over deno_core's timer machinery, id-tracked in `rt.*` | cancellation actually cancels; dropped runtimes drop their timers; no bare globals | timer left pending at exit can't hang the loop — ref'd holds until it fires, unref'd lets it exit, watchdog bounds the rest | 7 lifecycle tests, `79ef3a2` |
| 2.2 bounded fetch | streaming body cap, per-op deadline (`--fetch-timeout`/`OpBudgets`), error classification | a slow endpoint can't hold an op forever; a 500 is a result, not a crash | stall → deadline `HostError`(8), oversized → cut at the cap. *2026-11-16 fix:* the deadline covered `send()` only, so a body dripped a byte at a time held the op indefinitely; it now spans the whole exchange (trickle-body test) | 5 TCP-stub tests, `93b80b7` |
| 2.3 retained resources | `PendingCounts` snapshot from the activity tracker before isolate drop; `rt.pendingTimers()` script-side | teardown is complete — no zombie work; a leak is *seen*, not silently dropped | pending interval named even on a terminated run | 3 host tests + script-side count, `afbbef4` |

## M3 — capabilities

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 3.1 manifest complete (done) | paths, destinations, budgets declared per entry point — **and module resolution**: `JsrtModuleLoader` wraps `FsModuleLoader` and gates every non-entry file specifier at `resolve()`+`load()` through declared read paths + the entry's own dir (implicit program-space for `.js`/`.mjs` modules only; ops still manifest-only — *2026-11-16 fix:* text/bytes/JSON imports and any non-JS file imported as a module were also admitted from the entry dir, uncapped, so `jsrt run ~/x.js` exposed `~`; they now need a declared read path and stay under readText's 4 MiB cap, checked at `load()` where the requested type is known — dynamic-import roots arrive there without a referrer, so the entry is "no referrer **and** not dynamic"). Existing-outside → `CapabilityError` (7) naming the path; missing stays Load (3); `..`/symlinks fail closed via canonicalization; dynamic-import rejections lose their class at the V8 boundary, so `classify()` recognizes the loader's refusal prefix — uncaught still exits 7 | ops and the loader read the manifest fresh — never cached stale | manifest mid-run change is out of scope; boot-time parse errors fail fast | 9 lifecycle tests (outside, missing-still-3, grant-opens, deep-dep laundering, dynamic caught+uncaught, symlink, entry-ungated, grant-covers-both) — `d71f6e4`; plus 6 for the data-import fence (text, JSON and bytes beside the entry, a non-JS file imported as a module, a granted text+JSON import, the 4 MiB cap) — `4b04bc7` |
| 3.2 escape suite (done) | audit found a real hole: `url_authority` ended the authority only at `/`, so `http://evil.com\@granted.com/` checked `granted.com` while reqwest connected to `evil.com`. Authority now ends at `/ \ ? #`; host normalized (case, trailing dot, IPv6 brackets); grants parse `host`/`host:port`/`[v6]`/`[v6]:port`/bare v6, garbage never matches | the checked host is the connected host | every documented bypass refused | 5 unit suites + e2e pins: smuggled `\@` exits 7 naming the real destination, `file://` fetch refused — `f82f97e` |

## M4 — supervise

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 4.1 worker processes (done) | `jsrt supervise` spawns `jsrt run` in a child; CPU seconds by `RLIMIT_CPU` in pre_exec (child can't decline), RSS by `proc_pid_rusage` polling (25ms bound, not brick wall — documented), wall backstop, SIGTERM→SIGKILL; `wait4` returns status + peak rusage; JSON report: run_id, outcome, exit_code, signal, peak_rss, cpu_ms, limits. `RLIMIT_AS` rejected — V8's pointer cage makes it unbootable | the supervisor never dies with its worker — it only waits and reports | infinite loop → SIGXCPU at 1s classified `cpu-budget`, exit 6 | 6 e2e tests, `77f4c16` |
| 4.2 memory boundary (done — same evidence) | allocation bomb over `--max-rss-mb 200` → SIGTERM at ~231MB peak (poll-interval overshoot), `killed_by: rss-budget`, exit 6 | a memory bomb is an OS problem, not an in-process flag | the bomb was bounded, not the supervisor | bomb test in the same suite, `77f4c16` |
| 4.3 structured diagnostics (done) | per-run JSONL records via `--report` (run_id, outcome, exit_code, signal, peak_rss, cpu_ms, wall_ms, limits) under `--report-max-kb`; rotation to `<name>.1` opens with a meta-record carrying `dropped_records` — a flooded log limits itself and says so | diagnostics, not a workflow journal — one generation, visible drops. *2026-11-16 fix:* the worker's own stdout/stderr were inherited unbounded; they are now piped through per-stream forwarders capped by `--max-output-kb` (default 8 MiB) that keep draining past the cap and record `output.{stdout,stderr}_{bytes,dropped_bytes,tail_bytes}` in the report. *Review follow-up:* the notice was printed only at exit and everything past the cap was lost, including the host's final error line; the notice now goes out the moment a stream hits the cap, and the last 4 KiB of each capped stream is re-shown at exit (tests: flood capped per stream, cause of death survives, notice while running). Still open: redaction and run-id prefixes on worker lines, and a rate (bytes/s) budget for long-lived servers instead of a lifetime one | disk pressure / log storm → bounded output, supervisor preserved | flood test: 8 runs under 1 KiB cap → rotated, meta names the drop count, `pending`; worker output cap: 3 tests (flood capped per stream, cause of death survives the cap, notice while the worker runs) — `427cbab`, `8906c68` |

## M5 — stdlib wave 1

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 5.1 `rt.kv` (done) | embedded KV via rusqlite+bundled SQLite — get/put/delete/list behind a `"kv"` manifest line (`{path, max_bytes}`) or `--kv`/`--kv-max-mb`; the declared file **is** the namespace — two manifests on one path share a store, deliberately (persistence across runs is the point). Lazily-opened connection per run in OpState; sync ops (local-file SQLite is microseconds — an honest block beats a fake async); quota = db file's own size enforced at write with a page of slack; `list(prefix)` is an exact-prefix substr scan, not LIKE | KV ops are ops — capability line + evidence; no grant → no state cell → `CapabilityError` (7) | quota breach → `CapabilityError` naming file bytes/projection/grant | 6 e2e tests: refusal, roundtrip, cross-run persistence, quota, exact-prefix list, CLI grant — `47e01b1` |
| 5.2 `rt.media` (done — local backend; Bunny.net deferred, no credentials in scope) | streamed content-addressed uploads: `rt.media.put(srcPath)` streams a read-granted file (64KiB chunks, sha256 in flight) into the declared root under its hash name; `stat` → {name,bytes}|null; tmp renamed on success, removed on any failure — the store only holds complete name-matches-content files; dedup free (name IS content); `backend:"bunny"` fails at manifest parse, honestly unimplemented | upload streams — never a whole-file buffer; the source still needs a read grant (`is_read_allowed`, not `may_read` — its 4MiB cap is a readText materialization limit, not a stream limit) | mid-upload failure → partial removed, `CapabilityError`/`HostError` — never a stored claim; `uncertain` reserved for a network backend's mid-flight state | 7 e2e tests incl. over-cap abort leaving zero residue and dedup — `6ae471d` |

## M6 — operate a real app

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 6.1 `rt.serve` (done) | HTTP listener behind a `"serve"` manifest line (`{host, port, max_body}`, loopback IP literals only) or `--serve` — pull-model dispatch: hyper owns the wire (keep-alive, parsing, backpressure), the script's own event loop drives `await op_serve_next → handler → op_serve_respond`. Bounds: 16KiB head buffer, `max_body` → 413 pre-dispatch, 64-deep dispatch queue then TCP backpressure. *2026-11-16 fix:* there was no connection cap and no header/idle/body timeout (a slowloris held connections forever) and each request buffered its body before taking a queue slot; the grant now carries `max_connections` (permit before accept), `header_timeout_ms` (hyper timer, covers keep-alive idle) and `body_timeout_ms` (408). *Review follow-up:* reserving the queue slot before reading the body let 64 stalled uploads block every GET for the body timeout; bodies are now read first, under a per-listener byte budget (64 MiB, or `max_body` if larger) taken chunk by chunk, and only complete requests take a queue slot. *Second review pass:* taking that budget chunk by chunk while holding earlier chunks deadlocked concurrent uploads (24 x 7.5 MiB: 408s instead of 200s) and convoyed every POST behind stalled ones; now nothing waits for budget while holding any — the first 64 KiB of a body is free, a declared length is reserved whole up front, a chunked body takes budget per chunk, and a full budget is a 503 with `Retry-After` at once. *Third review pass:* a declared length held its reservation (and a pre-allocated buffer of that size) with no bytes sent, so 8 header-only connections denied large uploads for 30 s; a body now has `body_first_byte_ms` (5 s) to start and must average `body_min_rate` (64 KiB/s), or gets 408 and releases its share, and buffers grow with the bytes. And `max_connections` idle or slow sockets held every slot; at the cap the longest-idle connection without a request in the script is now closed to admit a newcomer. A connection slot was also held forever by a client that requested a large answer and never read it (hyper's header timer only covers reading heads); `write_timeout_ms` (default 30 s) now drops a connection whose response write makes no progress for that long. `deno_core::unsync::spawn` for listener tasks (tokio `spawn_local` panics — deno_core's executor is not a LocalSet) | a serve handler is capability-gated like everything else — no grant, no listener | a crashing handler is a JS exception in the host-owned loop → 500 on that request, host lives — proven: /boom → 500, next request → 201 | 6 e2e tests over raw TCP (refusals, dispatch, crash survival, pre-dispatch 413, flag path, non-loopback parse refusal) — `1b8271f` ; now 28 serve e2e tests plus 1 host test after the 2026-11-16 fixes, three review rounds and the verification pass (head over 16 KiB or 65+ header lines refused while the listener serves, eviction that a trickler cannot steer onto a response being written, small bodies outside the rate rule; idle declarers released at the first-byte deadline, no up-front allocation, longest-idle eviction at the cap and sparing busy requests, a refused upload without Expect still reading its 503, concurrent uploads never deadlock, a small POST beside stalled large ones, chunked fail-fast 503, close with keep-alive clients, suppressed-line count at death, connection cap and its bound, header/idle and body timeouts, 64 stalled bodies vs a GET, a slow reader, malformed answers, bad-answer log redaction, header arrays; u64 ids past 2^32) |
| 6.2 the webhook app (done) | `examples/webhook/` — a real app on `rt.serve` + `rt.kv` (dedup by delivery id, list by hook name) run under `jsrt supervise` through the whole drill | the guide's ship gate, verbatim: deploy → update → break → inspect → rollback | the drill exposed a real lifecycle bug — killing the supervisor orphaned the worker, which held the port and broke every respawn. Fixed: SIGTERM/SIGINT handlers armed *before* spawn, forwarded through the same TERM→KILL path the budgets use, recorded as `operator-sigterm`. Test kills on the supervisor's readiness line, never a timer | `drill.sh` + committed `drill.log`/`data/runs.jsonl`: v1→v2 data survives, broken build answers 500s, rollback restores v1+data, every respawn exits 6/signal-15, no orphans (port frees each step) |

## M7 — load and ship

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 7.1 cold start (done) | 1,000 release-binary spawns of `cold.js` → **p50 10.8ms / p95 14.4ms / p99 28.5ms**, max 138ms (one outlier, kept), 0 failures, peak child RSS 21.3MB | recorded machine + commit (`perf/results/m7-1-*`); laptop numbers stay lab numbers | regressions kept, not rerun | `perf/results/m7-1-cold-start-*` committed |
| 7.2 boundary + serve load (done) | **1,000,000 op calls** → 28.4ns/call (~25.8ns boundary after loop baseline, ~35.2M calls/s). **1,000,000 HTTP requests through `rt.serve`**: 1,000,000/1,000,000 ok, **90,251 rps @32 conns, p50 346µs / p95 430µs / p99 608µs, max 11.7ms, zero errors/refused/timeouts**. Saturation probe @128 conns: rps flat (90,347) while p50 → 1485µs — extra concurrency queues, doesn't buy throughput; the knee is the single-threaded pull-model dispatch loop | measured against the same build; misses kept, not rerun | overload behavior measured — saturation shown as latency growth at flat throughput, not a marketing rps | `perf/results/m7-2*` committed (incl. the broken-workload smoke artifact, kept) |
| 7.3 soak (done) | one hour, `soak.js`: setTimeout(0) churn + self-cancelling intervals + loopback fetch — **1,558,920 iterations, 389,730 fetches, 0 errors, pending_timers 9 at minute 1 and 9 at hour 1** (retained work fully recycled). RSS +52MB total but the shape is allocator steps, not a slope: flat ~30MB for 15min → one step to ~88MB → flat incl. a visible GC dip to 59MB → stepped down to ~82MB | failures are evidence — a leak climbs; this plateaus | growth investigated: pending count constant across the hour, RSS plateaus with a GC dip visible in the series | `perf/results/m7-3-soak-*` committed (incl. the broken-workload smoke runs) |

**Evidence that predates the 2026-11-16 security fixes.** The M7 numbers
above were measured on `ecbc7dd`/`e2a0099`, before the sandbox, fetch and
serve changes on the `fix/sandbox-security-2026-11-16` branch. They stay as
recorded, but they describe the old code paths and need a fresh run before
anyone quotes them for the current build: 7.2b serve 1M, the 128-connection
saturation probe and the composed webhook 1M (the listener now takes a
connection permit before accept, runs hyper's header timer and reserves a
queue slot before reading a body); 7.3 soak (its loopback fetch now goes
through the redirect-checking, public-address-only client under one
whole-exchange deadline); 7.1 cold start (the bootstrap now deletes the
`Deno`/`__bootstrap` globals); and the webhook drill (the supervisor now
pipes worker output through capped forwarders). 7.2a's `rt.now` op path is
unchanged in code, but its run should be repeated on the same build as the
rest. The verification pass below changed more of those paths again: serve
eviction (phase age, responses being written are protected), the body rate
rule (budget-holding bodies only), fetch slots (32 in flight) and a
near-heap-limit callback on every isolate.

## Security verification (2026-11-16)

Each defence below got a regression test that fails without it; a
defence that was missing was added in the same commit. Where it already
held, the guard was weakened locally to watch the test fail.

| Step | Outcome | Failure case before | Acceptance test |
|---|---|---|---|
| V.1 local modules only (done) | `JsrtModuleLoader` refuses every non-`file:` specifier at resolve and at load, as `CapabilityError` | `import 'https://…'` exited 3, an uncaught dynamic one 5: only deno_core's `FsModuleLoader` stopped them | static and dynamic `http:`/`https:`/`data:`/`blob:` imports exit 7, a counting stub sees zero connections — `05f69bf` |
| V.2 serve head bounds proven (done) | 16 KiB head and 64-header cap tested from both sides | none (defence held); with each guard weakened: 65 headers got 200, a 17 KiB head got 200 | `an_oversized_head_or_too_many_headers_is_refused_and_the_listener_keeps_serving` — `2eb518d` |
| V.3 no environment proxy proven (done) | `.no_proxy()` pinned by a test | none; with it removed, the body came "from the proxy" | `fetch_ignores_proxy_environment_variables` — `cf3a931` |
| V.4 fetch slots (done) | at most 32 exchanges in flight per run; later calls wait, their deadline starts at the slot | 1,000 parallel fetches had 526 in flight at once | `a_thousand_parallel_fetches_keep_in_flight_under_the_cap` — `570262f` |
| V.5 heap limit (done) | near-heap-limit callback terminates the isolate (exit 6); `--max-heap-mb` | a heap bomb ended in a V8 abort (signal 5, no exit code) | `an_allocation_bomb_under_jsrt_run_ends_classified_at_the_heap_limit` — `bd7cf34` |
| V.6 refusal messages (done) | messages name what the script asked for, never canonical paths or link targets | all six probes run against it leaked the canonical path | `caught_refusals_do_not_reveal_canonical_paths_or_symlink_targets` — `d4e91ce` |
| V.7 relative entry (done) | `jsrt run main.js` keeps its directory as module space | sibling imports refused, exit 7 | `a_bare_relative_entry_keeps_its_directory_as_program_space` — `e3aaaea` |
| V.8 unsafe confined (done) | `forbid(unsafe_code)` in jsrt-host and jsrt-load; every supervisor `unsafe` block has a `// SAFETY:` note | 10 undocumented blocks | Clippy `undocumented_unsafe_blocks = "deny"` — `6e1a3fd` |
| V.9 eviction by phase age (done, review 4-1) | at the cap, the connection longest in its current phase goes; a response being written is protected until flushed | a 16 MiB download paused 1.5 s was cut after ~1.3 MB while a trickler stayed | `at_the_cap_a_trickler_cannot_steer_eviction_onto_a_download_being_written` — `4b67507` |
| V.10 rate rule scope (done, review 4-2) | the first-byte + minimum-rate deadline applies only while a body holds budget | an 8 KiB body at 4 KiB/s got 408 at the rate deadline | `a_slow_small_body_gets_the_body_timeout_not_the_rate_deadline` — `cc03f74` |
| V.11 supply chain and CI (done) | `rust-toolchain.toml` 1.99.1; `scripts/check.sh`; full `cargo deny check`; CI on ubuntu-latest and macos-latest (SHA-pinned actions, read-only token); Dependabot | no CI; `cargo deny check` failed on 266 licence errors | the CI workflow itself — `7fcad0c`, `5fc88f5`, `a8deb5d` |

Review 4-3 (concurrent large uploads without `Expect: 100-continue` mostly
see a broken pipe, not the 503) needs no code change; the README says to
send `Expect`.

## Rejected / deferred, honestly

| Point | Why |
|---|---|
| Own script language / syntax | A compiler is a years-long separate project; the host stdlib is the "own backend" that actually ships |
| Deno/Node compat | Purpose-built host, not a compatibility target — per the guide |
| Memory sandbox in-process | `terminate_execution` stops CPU, and since V.5 a near-heap-limit callback ends a V8-heap bomb as exit 6; memory outside the V8 heap (`ArrayBuffer` backing stores, host buffers) still needs a process boundary — that's why M4 exists |
| Durable workflow journal | Diagnostic logs are not a journal — that lives in project 04 (Durable Workflow Runner) |
| V8 inspector (devtools) | In the guide's full direction, but value shows up once real apps run on it — lands alongside M6's operate work if needed, not before |
| TypeScript transformation | Separate contract per the guide — scripts are JavaScript; TS support is a stated later item, not silently assumed |

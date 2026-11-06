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
| 2.2 bounded fetch | streaming body cap, per-op deadline (`--fetch-timeout`/`OpBudgets`), error classification | a slow endpoint can't hold an op forever; a 500 is a result, not a crash | stall → deadline `HostError`(8), oversized → cut at the cap | 5 TCP-stub tests, `93b80b7` |
| 2.3 retained resources | `PendingCounts` snapshot from the activity tracker before isolate drop; `rt.pendingTimers()` script-side | teardown is complete — no zombie work; a leak is *seen*, not silently dropped | pending interval named even on a terminated run | 3 host tests + script-side count, `afbbef4` |

## M3 — capabilities

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 3.1 manifest complete (done) | paths, destinations, budgets declared per entry point — **and module resolution**: `JsrtModuleLoader` wraps `FsModuleLoader` and gates every non-entry file specifier at `resolve()`+`load()` through declared read paths + the entry's own dir (implicit program-space for modules only; ops still manifest-only). Existing-outside → `CapabilityError` (7) naming the path; missing stays Load (3); `..`/symlinks fail closed via canonicalization; dynamic-import rejections lose their class at the V8 boundary, so `classify()` recognizes the loader's refusal prefix — uncaught still exits 7 | ops and the loader read the manifest fresh — never cached stale | manifest mid-run change is out of scope; boot-time parse errors fail fast | 9 lifecycle tests (outside, missing-still-3, grant-opens, deep-dep laundering, dynamic caught+uncaught, symlink, entry-ungated, grant-covers-both) — `d71f6e4` |
| 3.2 escape suite (done) | audit found a real hole: `url_authority` ended the authority only at `/`, so `http://evil.com\@granted.com/` checked `granted.com` while reqwest connected to `evil.com`. Authority now ends at `/ \ ? #`; host normalized (case, trailing dot, IPv6 brackets); grants parse `host`/`host:port`/`[v6]`/`[v6]:port`/bare v6, garbage never matches | the checked host is the connected host | every documented bypass refused | 5 unit suites + e2e pins: smuggled `\@` exits 7 naming the real destination, `file://` fetch refused — `f82f97e` |

## M4 — supervise

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 4.1 worker processes (done) | `jsrt supervise` spawns `jsrt run` in a child; CPU seconds by `RLIMIT_CPU` in pre_exec (child can't decline), RSS by `proc_pid_rusage` polling (25ms bound, not brick wall — documented), wall backstop, SIGTERM→SIGKILL; `wait4` returns status + peak rusage; JSON report: run_id, outcome, exit_code, signal, peak_rss, cpu_ms, limits. `RLIMIT_AS` rejected — V8's pointer cage makes it unbootable | the supervisor never dies with its worker — it only waits and reports | infinite loop → SIGXCPU at 1s classified `cpu-budget`, exit 6 | 6 e2e tests, `77f4c16` |
| 4.2 memory boundary (done — same evidence) | allocation bomb over `--max-rss-mb 200` → SIGTERM at ~231MB peak (poll-interval overshoot), `killed_by: rss-budget`, exit 6 | a memory bomb is an OS problem, not an in-process flag | the bomb was bounded, not the supervisor | bomb test in the same suite, `77f4c16` |
| 4.3 structured diagnostics (done) | per-run JSONL records via `--report` (run_id, outcome, exit_code, signal, peak_rss, cpu_ms, wall_ms, limits) under `--report-max-kb`; rotation to `<name>.1` opens with a meta-record carrying `dropped_records` — a flooded log limits itself and says so | diagnostics, not a workflow journal — one generation, visible drops | disk pressure / log storm → bounded output, supervisor preserved | flood test: 8 runs under 1 KiB cap → rotated, meta names the drop count, `pending` |

## M5 — stdlib wave 1

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 5.1 `rt.kv` (done) | embedded KV via rusqlite+bundled SQLite — get/put/delete/list behind a `"kv"` manifest line (`{path, max_bytes}`) or `--kv`/`--kv-max-mb`; the declared file **is** the namespace — two manifests on one path share a store, deliberately (persistence across runs is the point). Lazily-opened connection per run in OpState; sync ops (local-file SQLite is microseconds — an honest block beats a fake async); quota = db file's own size enforced at write with a page of slack; `list(prefix)` is an exact-prefix substr scan, not LIKE | KV ops are ops — capability line + evidence; no grant → no state cell → `CapabilityError` (7) | quota breach → `CapabilityError` naming file bytes/projection/grant | 6 e2e tests: refusal, roundtrip, cross-run persistence, quota, exact-prefix list, CLI grant — `47e01b1` |
| 5.2 `rt.media` (done — local backend; Bunny.net deferred, no credentials in scope) | streamed content-addressed uploads: `rt.media.put(srcPath)` streams a read-granted file (64KiB chunks, sha256 in flight) into the declared root under its hash name; `stat` → {name,bytes}|null; tmp renamed on success, removed on any failure — the store only holds complete name-matches-content files; dedup free (name IS content); `backend:"bunny"` fails at manifest parse, honestly unimplemented | upload streams — never a whole-file buffer; the source still needs a read grant (`is_read_allowed`, not `may_read` — its 4MiB cap is a readText materialization limit, not a stream limit) | mid-upload failure → partial removed, `CapabilityError`/`HostError` — never a stored claim; `uncertain` reserved for a network backend's mid-flight state | 7 e2e tests incl. over-cap abort leaving zero residue and dedup — `6ae471d` |

## M6 — operate a real app

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 6.1 `rt.serve` (done) | HTTP listener behind a `"serve"` manifest line (`{host, port, max_body}`, loopback IP literals only) or `--serve` — pull-model dispatch: hyper owns the wire (keep-alive, parsing, backpressure), the script's own event loop drives `await op_serve_next → handler → op_serve_respond`. Bounds: 16KiB head buffer, `max_body` → 413 pre-dispatch, 64-deep dispatch queue then TCP backpressure. `deno_core::unsync::spawn` for listener tasks (tokio `spawn_local` panics — deno_core's executor is not a LocalSet) | a serve handler is capability-gated like everything else — no grant, no listener | a crashing handler is a JS exception in the host-owned loop → 500 on that request, host lives — proven: /boom → 500, next request → 201 | 6 e2e tests over raw TCP (refusals, dispatch, crash survival, pre-dispatch 413, flag path, non-loopback parse refusal) — `1b8271f` |
| 6.2 the webhook app (done) | `examples/webhook/` — a real app on `rt.serve` + `rt.kv` (dedup by delivery id, list by hook name) run under `jsrt supervise` through the whole drill | the guide's ship gate, verbatim: deploy → update → break → inspect → rollback | the drill exposed a real lifecycle bug — killing the supervisor orphaned the worker, which held the port and broke every respawn. Fixed: SIGTERM/SIGINT handlers armed *before* spawn, forwarded through the same TERM→KILL path the budgets use, recorded as `operator-sigterm`. Test kills on the supervisor's readiness line, never a timer | `drill.sh` + committed `drill.log`/`data/runs.jsonl`: v1→v2 data survives, broken build answers 500s, rollback restores v1+data, every respawn exits 6/signal-15, no orphans (port frees each step) |

## M7 — load and ship

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 7.1 cold start (done) | 1,000 release-binary spawns of `cold.js` → **p50 10.8ms / p95 14.4ms / p99 28.5ms**, max 138ms (one outlier, kept), 0 failures, peak child RSS 21.3MB | recorded machine + commit (`perf/results/m7-1-*`); laptop numbers stay lab numbers | regressions kept, not rerun | `perf/results/m7-1-cold-start-*` committed |
| 7.2 boundary + serve load (done) | **1,000,000 op calls** → 28.4ns/call (~25.8ns boundary after loop baseline, ~35.2M calls/s). **1,000,000 HTTP requests through `rt.serve`**: 1,000,000/1,000,000 ok, **90,251 rps @32 conns, p50 346µs / p95 430µs / p99 608µs, max 11.7ms, zero errors/refused/timeouts**. Saturation probe @128 conns: rps flat (90,347) while p50 → 1485µs — extra concurrency queues, doesn't buy throughput; the knee is the single-threaded pull-model dispatch loop | measured against the same build; misses kept, not rerun | overload behavior measured — saturation shown as latency growth at flat throughput, not a marketing rps | `perf/results/m7-2*` committed (incl. the broken-workload smoke artifact, kept) |
| 7.3 soak | one-hour timers/network workload → retained handles, RSS drift | failures are evidence | any growth investigated and written up | committed results |

## Rejected / deferred, honestly

| Point | Why |
|---|---|
| Own script language / syntax | A compiler is a years-long separate project; the host stdlib is the "own backend" that actually ships |
| Deno/Node compat | Purpose-built host, not a compatibility target — per the guide |
| Memory sandbox in-process | `terminate_execution` stops CPU; only a process boundary stops allocation — that's why M4 exists |
| Durable workflow journal | Diagnostic logs are not a journal — that lives in project 04 (Durable Workflow Runner) |
| V8 inspector (devtools) | In the guide's full direction, but value shows up once real apps run on it — lands alongside M6's operate work if needed, not before |
| TypeScript transformation | Separate contract per the guide — scripts are JavaScript; TS support is a stated later item, not silently assumed |

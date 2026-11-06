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
| 4.1 worker processes | isolate per process, supervised; memory + CPU budgets enforced by the OS | the supervisor never dies with its worker | infinite loop → killed inside budget + grace | supervision drill |
| 4.2 memory boundary | near-heap-limit → controlled death, classified `terminated` | a memory bomb is an OS problem, not an in-process flag | allocation bomb dies bounded, supervisor reports it | bomb test with evidence |
| 4.3 structured diagnostics | per-run JSON log records — run ID, timestamps, outcome class, resource use — with byte limits, rotation, and a visible dropped-record count | a flooded log limits itself predictably and says so; diagnostics are never confused with a workflow journal | disk pressure / log storm → bounded output, supervisor preserved, failure exposed not hidden | flood test: records dropped+counted, host survives |

## M5 — stdlib wave 1

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 5.1 `rt.kv` | embedded KV (SQLite) — get/put/delete/list, per-script namespace | KV ops are ops — capability line + evidence | quota + contention honest | API + tests |
| 5.2 `rt.media` | streaming upload abstraction; Bunny.net edge + local-disk backend, content-addressed names | upload streams — never a whole-file buffer; retry evidence per attempt | mid-upload failure → `uncertain`, never `uploaded` | live drill vs Bunny sandbox or local backend |

## M6 — operate a real app

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 6.1 `rt.serve` | HTTP listener op — the webhook app's foundation | a serve handler is capability-gated like everything else | crash in a handler can't take the host down | serve + request tests |
| 6.2 the webhook app | deployed, updated, broken, inspected, rolled back — on jsrt itself | the guide's ship gate, verbatim | each drill step is committed evidence | deploy drill log |

## M7 — load and ship

| Step | Outcome | Invariant | Failure case | Acceptance test |
|---|---|---|---|---|
| 7.1 cold start | 1,000 short scripts → p50/p95 startup + steady RSS | recorded machine + commit; laptop numbers stay lab numbers | regressions kept, not rerun | committed results |
| 7.2 boundary | 10,000 host calls → op overhead | measured against the same build | — | committed results |
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

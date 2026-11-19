# jsrt load matrix — stability and scale evidence

Goal: committed evidence the host holds under million-scale work — the op boundary, the HTTP
listener, supervised runs, timers — per the workspace standard (units: op calls, HTTP
requests, spawns). Evidence lands in `perf/results/` with machine + commit; misses kept.

Honesty: numbers are development observations on this machine, not production-capacity claims.

| Module | Scenario | What it proves | Evidence status |
|---|---|---|---|
| Op boundary | `perf/load.py op` | 1M op calls — boundary cost, no drift | committed (1M, ~28ns/call); re-run 2026-11-17 at `f83f39d`: 27.8 ns/call |
| `rt.serve` | `perf/load.py serve` + `saturation` | 1M requests through the pull-model listener; overload = latency growth at flat throughput, not collapse | committed (1M ok, 90,251 rps, saturation probed @128 conns); re-run 2026-11-17 at `f83f39d`: 1M ok, 86,620 rps @32, saturation 85,875 rps @128, zero errors (`load.py serve`, `load.py saturation`) |
| Cold start | `perf/load.py cold` | release-binary spawn cost | committed (1000 spawns, p50 10.8ms); re-run 2026-11-17 at `f83f39d`: p50 9.28 ms, p99 11.35 ms, 0 failures |
| Composed app | `perf/load.py webhook` (`jsrt-load` POST vs `examples/webhook`) | rt.serve + rt.kv real app at million scale | committed (1M composed); re-run 2026-11-17 at `f83f39d` (`load.py webhook`): 1M ok, 50,629 rps, dedup held |
| Supervision | `perf/load.py churn` — spawn→ready→SIGTERM cycles of a serve worker | worker lifecycle under load — no orphans, budgets enforced | defined (1,000 rounds at full scale); local run committed 2026-11-19 at `b1d07a6` (`JSRT_LOAD_UNITS=100`): 100/100 exit 6 classified operator-sigterm, 0 orphans, 0 respawn failures |
| `rt.kv` | `perf/load.py kv` — mixed get/put/list/delete against a declared store + quota pressure | get/put/list at rate — quota enforcement under pressure, reads that actually hit | defined (1M ops at full scale); local run committed 2026-11-19 at `b1d07a6` (`JSRT_LOAD_UNITS=50000`): 50,000 ops, 18,750/18,750 gets hit, 0 integrity fails, 200 named-capability quota refusals, post-quota writes ok |
| `rt.media` | `perf/load.py media` — streamed puts over distinct sources + dedup + stat | streamed puts at rate — dedup, partial cleanup, complete files only | defined (100k puts at full scale); local run committed 2026-11-19 at `b1d07a6` (`JSRT_LOAD_UNITS=2000`): 2,000 puts over 64 sources, 64 stored files / 392 KiB, 0 stray tmp, every stat ok |
| Refusal flood | `perf/load.py refuse` — ungated op calls at rate | refusals classify + exit fast, no leak | defined (1M calls at full scale); local run committed 2026-11-19 at `b1d07a6` (`JSRT_LOAD_UNITS=100000`): 100,000 attempts, 100,000 named CapabilityError, 0 unexpected successes, 430 ms |
| Timers/lifecycle | `perf/soak.py` | timer churn + self-cancelling intervals + loopback fetch — pending counts recycle | committed (1h: 1.56M iterations, pending flat); re-run 2026-11-17 at `f83f39d`: 1.44M iterations, 0 fetch errors, pending 9 → 9, RSS flat after one step |
| **Extended soak** | `perf/soak.py` at 24h | optional hardening — hour soak clean; a day is the stronger claim | optional |

## Gaps to close this pass

1. ~~Define + run the four missing scenarios~~ — done 2026-11-19: scenarios defined in
   `perf/load.py` with declared thresholds; local reduced-scale runs committed under
   `perf/results/m8-*.json`. Full-scale runs are wired in `.github/workflows/load.yml`
   (`workflow_dispatch` only, ubuntu-24.04, artifacts uploaded, nothing pushed back).
   Earlier kept run: `m8-1-kv-sustained-2026-11-18T23-52-02Z` passed its checks but its
   reads never hit (gets/puts targeted disjoint keys) — retained as a miss against the
   corrected workload; superseded by `...23-59-39Z`.
2. Optionally extend the soak to 24h — the hour result is clean; a day strengthens the claim.

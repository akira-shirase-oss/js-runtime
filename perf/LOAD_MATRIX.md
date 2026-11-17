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
| Supervision | sustained `jsrt supervise` spawn/respawn churn | worker lifecycle under load — no orphans, budgets enforced | gap — define sustained run |
| `rt.kv` | kv sustained scenario (define) | get/put/list at rate against a declared store — quota enforcement under pressure | gap — define + run |
| `rt.media` | media sustained scenario (define) | streamed puts at rate — dedup, partial cleanup under churn | gap — define + run |
| Refusal flood | capability-denial scenario (define) | ungated op calls at rate — refusals classify + exit fast, no leak | gap — define + run |
| Timers/lifecycle | `perf/soak.py` | timer churn + self-cancelling intervals + loopback fetch — pending counts recycle | committed (1h: 1.56M iterations, pending flat); re-run 2026-11-17 at `f83f39d`: 1.44M iterations, 0 fetch errors, pending 9 → 9, RSS flat after one step |
| **Extended soak** | `perf/soak.py` at 24h | optional hardening — hour soak clean; a day is the stronger claim | optional |

## Gaps to close this pass

1. Define + run the four missing scenarios (kv, media, refusal flood, supervise churn) —
   extend `jsrt-load`/existing drivers; commit results.
2. Optionally extend the soak to 24h — the hour result is clean; a day strengthens the claim.

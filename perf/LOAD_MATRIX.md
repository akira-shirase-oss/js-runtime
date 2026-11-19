# jsrt load matrix — stability and scale evidence

Goal: committed evidence the host holds under million-scale work — the op boundary, the HTTP
listener, supervised runs, timers — per the workspace standard (units: op calls, HTTP
requests, spawns). Evidence lands in `perf/results/` with machine + commit; misses kept.

Honesty: numbers are development observations on this machine, not production-capacity claims.

| Module | Scenario | What it proves | Evidence status |
|---|---|---|---|
| Op boundary | `perf/load.py op` | 1M op calls — boundary cost, no drift | **full-scale CI 2026-11-19 at `8dc8d32`** (workflow `load.yml`, ubuntu-24.04, Xeon Platinum 8573C 4c): 1,000,000 calls, 51.5 ns/call (47.1 ns after loop baseline), passed. Earlier: ~28 ns/call on the M2 Pro at `f83f39d` |
| `rt.serve` | `perf/load.py serve` + `saturation` | 1M requests through the pull-model listener; overload = latency growth at flat throughput, not collapse | **full-scale CI 2026-11-19 at `8dc8d32`**: 1M/1M ok, 75,148 rps @32 (p50 424 µs); saturation 76,619 rps @128 (p50 1,710 µs) — flat throughput, latency queues, zero errors. Earlier: 86,620 / 85,875 rps on the M2 Pro at `f83f39d` |
| Cold start | `perf/load.py cold` | release-binary spawn cost | **full-scale CI 2026-11-19 at `8dc8d32`**: 1,000 spawns, p50 14.6 ms, p99 21.7 ms, 0 failures. Earlier: p50 9.28 ms on the M2 Pro at `f83f39d` |
| Composed app | `perf/load.py webhook` (`jsrt-load` POST vs `examples/webhook`) | rt.serve + rt.kv real app at million scale | **full-scale CI 2026-11-19 at `8dc8d32`**: 1M POSTs, 41,551 rps, zero errors, dedup held. Earlier: 50,629 rps on the M2 Pro at `f83f39d` |
| Supervision | `perf/load.py churn` — spawn→ready→SIGTERM cycles of a serve worker | worker lifecycle under load — no orphans, budgets enforced | **full-scale CI 2026-11-19 at `8dc8d32`**: 1,000/1,000 rounds exit 6 classified operator-sigterm per the scenario's own per-round read-back, 1,000 records retained across both report generations (one rotation), 0 orphans, 0 respawn failures. Superseded miss kept: `m8-4-...00-21-12Z-067e974` (passed:false — the old check read only the live report file; the displaced generation in `<name>.1` held the rest; bounded by design, nothing lost) |
| `rt.kv` | `perf/load.py kv` — mixed get/put/list/delete against a declared store + quota pressure | get/put/list at rate — quota enforcement under pressure, reads that actually hit | **full-scale CI 2026-11-19 at `8dc8d32`**: 1,000,000 ops in 623 s, 375,000/375,000 gets hit, 0 integrity fails, 200 named-capability quota refusals, post-quota writes ok. Superseded miss kept: `m8-1-...23-52-02Z` (reads never hit — disjoint keys; workload corrected) |
| `rt.media` | `perf/load.py media` — streamed puts over distinct sources + dedup + stat | streamed puts at rate — dedup, partial cleanup, complete files only | **full-scale CI 2026-11-19 at `8dc8d32`**: 100,000 puts / 64 distinct sources in 63 s (~613 MiB streamed), 64 stored files / 392 KiB after dedup, 0 stray tmp, every stat ok. Superseded miss kept: `m8-2-...00-00-02Z` (harness created no `--media-root` dir — refused, fixed) |
| Refusal flood | `perf/load.py refuse` — ungated op calls at rate | refusals classify + exit fast, no leak | **full-scale CI 2026-11-19 at `8dc8d32`**: 1,000,000 attempts → 1,000,000 named CapabilityError, 0 unexpected successes, 6.7 s |
| Timers/lifecycle | `perf/soak.py` | timer churn + self-cancelling intervals + loopback fetch — pending counts recycle | committed (1h: 1.56M iterations, pending flat); re-run 2026-11-17 at `f83f39d`: 1.44M iterations, 0 fetch errors, pending 9 → 9, RSS flat after one step |
| **Extended soak** | `perf/soak.py` at 24h | optional hardening — hour soak clean; a day is the stronger claim | optional |

## Gaps to close this pass

1. ~~Define + run the four missing scenarios~~ — done 2026-11-19: all nine scenarios
   ran at full scale on CI (`load.yml` run 44429626909, commit `8dc8d32`) and passed;
   artifacts committed under `perf/results/*-8dc8d32.json`. `load.py` exits non-zero
   when any scenario's checks fail (`perf/test_load.py` covers it) — the earlier run
   that showed SUCCESS over a `passed:false` artifact cannot repeat. Kept misses
   (never deleted): `m8-1-...23-52-02Z` (reads never hit), `m8-2-...00-00-02Z`
   (harness didn't create the media root), `m8-4-...00-21-12Z-067e974` (the
   passed:false run that drove the bounded-report check fix).
2. Optionally extend the soak to 24h — the hour result is clean; a day strengthens the claim.

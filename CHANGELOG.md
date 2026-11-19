# Changelog

## [1.0.0] — 2026-11-19

First release of jsrt: a purpose-built JavaScript host. V8 (through
`deno_core =0.413.0`) executes scripts; the Rust host owns capabilities,
lifecycle and honest error classification.

### The runtime

- `jsrt run` / `jsrt supervise`. Exactly one script namespace — `rt.*`;
  no ambient `Deno`, no ungated global.
- Deny-by-default manifest (`jsrt.manifest.json`): `read`, `net`,
  `kv`, `media`, `serve` grants; CLI flags only ever extend it.
- `rt.print`, `rt.hostInfo`, `rt.now`, `rt.sleep`, Node-style timers
  (`setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`,
  `unrefTimer`/`refTimer`, `rt.pendingTimers`), `rt.readText` (4 MiB
  cap, regular files only), `rt.fetch` (declared destinations, redirect
  re-checks, no environment proxies, 32 exchanges in flight, 4 MiB body
  cap), `rt.kv` (embedded SQLite, quota-enforced), `rt.media`
  (content-addressed streamed puts), `rt.serve` (pull-model loopback
  listener with head/body/rate/connection bounds).
- Exit-code contract: 0 ok, 2 usage, 3 load, 4 syntax, 5 exception,
  6 terminated, 7 capability, 8 host. Terminated is never confused with
  thrown.
- A real timeout: the watchdog calls `terminate_execution` on the
  deadline; the V8 heap has a near-limit callback that ends a heap bomb
  as exit 6 instead of a V8 abort.

### Supervision (`jsrt supervise`)

- Process boundary for what the V8 heap cannot bound: `RLIMIT_CPU`, RSS
  polling, SIGTERM→SIGKILL escalation, bounded JSONL run reports.
- Worker output bounded per stream: a lifetime cap (`--max-output-kb`,
  default 8 MiB) that doubles as a token-bucket burst once a sustained
  rate is set (`--max-output-rate-kbs`); the last 4 KiB is re-shown at
  exit so the cause of death survives.
- Every forwarded worker stderr line carries `[run <id>]`; worker
  stdout stays byte-exact unless `--prefix-stdout` asks otherwise.
- Secret redaction on forwarded output: `--redact-env NAME`,
  `--redact-file PATH` (never a secret on argv); literals are masked
  across chunk boundaries, overlapping secrets mask their union, and
  the configuration is bounded (4-byte minimum, 256 secrets, 64 KiB
  total). Counts land in the report; values never do.
- When the sink is a terminal, worker control bytes are neutralised
  (UTF-8-aware: valid scalars pass byte-for-byte; C0/C1/DEL and stray
  bytes render visibly). Pipes stay byte-exact; `--output-raw` opts out
  for trusted workers (redaction still applies).
- External signals are reported honestly — "signal 15 (sender
  unknown)" — never implied to be jsrt's own.

### Security posture

- Deny by default everywhere; refusal is a named `CapabilityError`
  scripts can match on, and refusal messages name what the script asked
  for — never a canonical host path or symlink target.
- Only local `.js`/`.mjs` files are modules; `http:`/`https:`/`data:`/
  `blob:` imports are refused by jsrt's own loader.
- Name grants reach public addresses only unless `"private": true` or
  `--net-private` opts in; environment proxies are ignored.
- 2026-11-16 security verification: every defence has a regression test
  that fails without it (see README "Security verification"). Behaviour
  differences from earlier development builds are listed under
  "Breaking changes" there.
- Known, documented limits (README "Still open"): memory outside the V8
  heap is bounded only by `jsrt supervise --max-rss-mb`; one call
  allocating >1 GiB past the heap limit can still abort V8; unsupervised
  `jsrt run` output is uncapped; `.js`/`.mjs` files under the entry
  directory are importable and enumerable by the script.

### Evidence

- Load matrix (`perf/LOAD_MATRIX.md`): op boundary 1M calls
  (~27.8 ns/call), `rt.serve` 1M requests (~86.6k rps, saturation
  probed), 1,000 cold starts (p50 9.28 ms), 1M composed webhook
  requests (~50.6k rps), a one-hour timers/lifecycle soak — plus the
  four scenarios defined this pass: kv sustained, media sustained,
  capability refusal flood and supervise churn, with declared
  thresholds and committed reduced-scale results; full-scale runs are a
  `workflow_dispatch` CI job (`.github/workflows/load.yml`).
- Webhook drill (`examples/webhook/drill.sh`) re-run on this build:
  deploy, update, break, inspect, rollback — all five phases green.
- Numbers are development observations on the machine that produced
  them, recorded with machine + commit; not production-capacity claims.

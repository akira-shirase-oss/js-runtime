# jsrt — agent instructions

A purpose-built JavaScript host in Rust. V8 (via `deno_core`, pinned
`=0.413.0`) executes scripts; the Rust host owns capabilities, lifecycle and
honest errors. This is a bounded systems-learning project: **nothing else in
the portfolio may depend on it until it is proven.**

## Non-negotiables

1. **Deny by default.** A script can only do what an op grants; a capability
   only works if the manifest declares it. New op → new manifest line → new
   refusal test, in the same change.
2. **Classify, don't stringify.** Every way a run ends has a `HostError`
   variant and an exit code (0/2/3/4/5/6/7/8). Terminated ≠ exception; a
   capability refusal is a named `CapabilityError` scripts can match.
3. **The engine is a dependency.** Pin it (`=0.413.0`), don't wrap it
   aspirationally. No rewriting V8 behaviors; fix our host instead.
4. **Bounded everything.** Deadlines on fetch, a body cap, a run timeout,
   a read cap. New capability → its bound, documented.
5. **Evidence over claims.** `perf/` numbers carry machine + commit. Laptop
   numbers are development observations, never public benchmarks. Load runs
   are aggressive per the workspace rule: million-scale where a traffic unit
   exists (HTTP requests through `rt.serve`, op calls, supervised runs),
   results committed with machine + commit, misses kept not rerun.
6. **`JsRuntime` is `!Send`** — current-thread executor everywhere (CLI and
   tests). CPU-bound termination goes through `terminate_execution` on a
   watchdog thread; a tokio timeout alone cannot preempt running JS.
7. **Memory is an OS problem.** In-process flags can't stop allocation bombs
   — that's what the M4 supervisor is for; don't pretend otherwise.

## Conventions

- `rust-mastery` skill rules apply: `export PATH="$(brew --prefix rustup)/bin:$PATH"`
  before every cargo command; workspace lints deny `unwrap`/`expect` — tests
  use `unwrap_or_else(|| panic!(...))`.
- Extension JS (`js/*.js`) must be **7-bit ASCII** — deno_core asserts it;
  use `--` not em-dashes in JS comments.
- Builtin op names are taken (`op_print`, `op_read`, `op_timer_*`) — ours
  must not collide; check `ops_builtin.rs` when adding ops.
- Op2 gotchas (deno_core 0.413): async ops are plain `#[op2] async fn`
  (no marker); `String` returns need `#[string]`; serde returns need
  `#[serde]` on the fn + a named `Serialize` struct (not `serde_json::Value`);
  state arrives as `state: Rc<RefCell<OpState>>`.
- Test flavor: `#[tokio::test(flavor = "current_thread")]`.
- Load evidence: `crates/jsrt-load` is the HTTP driver (keep-alive workers,
  JSON report; `--method/--header/--body` supported). `perf/load.py` runs
  the cold/op/serve scenarios, `perf/soak.py` the soak; artifacts land in
  `perf/results/` — never delete a bad run.
- `jsrt supervise` lifecycle: signals forward to the worker; the stderr
  line `worker N running, signals forwarded` is the readiness marker —
  tests kill on it, never on a sleep.

## Verification gate (run before every commit)

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Plus, when the CLI changed: the exit-code spot check —
`jsrt run scripts/hello.js` → 0; missing → 3; bad syntax → 4; thrower → 5.

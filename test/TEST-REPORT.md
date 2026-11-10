# Test report: jsrt (`js-runtime`)

**Commit tested:** `c30ac2e` (guide written against this HEAD; report committed on top)
**Date:** 2026-11-10 · **Machine:** Apple M2 Pro, 12 cores, 32 GB, macOS (Darwin 27.0.0)
**Toolchain:** Rust 1.99.1 via rustup proxies · k6 not needed (native `jsrt-load` driver)
**Guide:** `test/js-runtime-TESTING.md`
**Verdict: PASS — no product bugs found.** One documentation commit (`aa0d7d9`) added the
guide and refreshed the webhook-drill evidence artifacts.

---

## 1. Build and workspace tests

| Check | Command | Result |
|---|---|---|
| Build | `cargo build` | clean |
| Tests | `cargo test --workspace --locked` | **84 passed, 0 failed** (57 lifecycle + 6 serve + 8 supervise + 13 host) |
| Format | `cargo fmt --all --check` | clean |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` | clean |

The 2 doc-tests under `deno_core` report `ignored` (upstream, expected).

## 2. Happy path

- `jsrt run scripts/hello.js` → exit **0**, prints `hello from jsrt 0.1.0 on v8`,
  `answer: 42`, `slept 10ms and returned`.
- Uncaught `CapabilityError` is *catchable*: a script that wraps the denied call in
  try/catch prints `caught: CapabilityError` and exits **0** — refusals are values the
  script can match, not process deaths.

## 3. Exit-code contract (every code exercised)

| Exit | Trigger | Observed |
|---|---|---|
| 0 | `hello.js` success; caught CapabilityError | `exit=0` both |
| 2 | missing `<FILE>` arg; unknown subcommand `bogus` | clap usage error, `exit=2` |
| 3 | `jsrt run scripts/nope.js` | `load: no such file`, `exit=3` |
| 4 | syntax-error script | `syntax: SyntaxError: Unexpected token ';'`, `exit=4` |
| 5 | script throws `Error: boom` | `exception: Error: boom`, `exit=5` |
| 6 | `--timeout` watchdog; stalled promise (`run did not unwind 2000ms after termination`) | `terminated: …`, `exit=6` |
| 7 | uncaught `CapabilityError` (`rt.readText("/etc/passwd")`, no manifest) | `capability: CapabilityError: read denied: … is outside every declared read path`, `exit=7` — **distinct from generic exception (5)** |
| 8 | `rt.serve` manifest/flag host violations | `host: manifest serve host 'localhost' is not an IP literal`, `exit=8` |

## 4. Capability fences (break-it)

**Filesystem**
- No manifest → deny-all: `rt.readText` of any path refuses.
- Path escape `../` beyond declared roots → `CapabilityError` (caught in-script, exit 0).
- **Symlink escape**: a symlink inside a granted dir pointing outside → refused.
- Manifest grants `data/` only; CLI `--read` joins an extra dir — with the flag the extra
  file reads, without it the same file refuses. `secret.txt` outside every grant refuses
  in both configurations.

**Network (`rt.fetch`)**
- No grant → `fetch denied: 127.0.0.1:PORT is not a declared net destination`.
- `file:///etc/passwd` → `fetch denied: unparseable URL` (scheme fence).
- Granted host:port → `status=200` against a live `python3 -m http.server`.
- A *different* granted host does not cover the target — per-authority matching, not
  substring.

**`rt.serve` bind rules**
- `localhost` → exit 8 (must be an IP literal).
- `8.8.8.8` → exit 8 (`not loopback — v1 binds loopback only`).
- `0.0.0.0` → exit 8 (same).

## 5. Supervision budgets (`jsrt supervise`)

| Drill | Budget | Observed |
|---|---|---|
| RSS bomb (grows a buffer) | `--max-rss-mb 128` | worker killed by `rss-budget` at peak 152,190,976 B (~145 MiB), `exit_code:6`, SIGTERM, wall 3997 ms — JSON report records `outcome:"terminated"`, `killed_by`, `cpu_ms`, `peak_rss_bytes`, limits |
| CPU hog (busy loop) | `--max-cpu 2` | killed by `cpu-budget` via RLIMIT_CPU/SIGXCPU at `cpu_ms:2002`, `exit_code:6` |
| Report | `--report` | JSONL append with run_id, outcome, exit_code, worker_pid, wall/cpu, peak RSS, limits — used by the webhook drill's INSPECT step |

## 6. Webhook drill (`examples/webhook/drill.sh`, refreshed in `aa0d7d9`)

Full deploy → update → break → inspect → rollback cycle against a real `jsrt supervise`
worker serving `rt.serve` + `rt.kv`:

1. **DEPLOY v1** — `health: {"ok":true,"v":1}`; `post d1` → `duplicate` (KV dedup by
   delivery id); second post of same id → `duplicate` again; `post d2` stored;
   `/hooks/github` lists all keys.
2. **UPDATE v2** — swap `current.js`, respawn: `{"ok":true,"v":2}`; KV data **survived**
   the restart (`["hook/github/closeout","hook/github/d1","hook/github/d2"]`).
3. **BREAK** — handler throws on every route: health and posts return **500** while the
   wire/supervisor stay up (host survives a handler exception).
4. **INSPECT** — report file holds 6 run records across deploy/update/break.
5. **ROLLBACK** — restore v1: `{"ok":true,"v":1}`, prior KV data intact, 8 total records.

`DRILL COMPLETE`. Artifacts (`runs.jsonl`, `drill.log`) refreshed and committed.

## 7. Prior load evidence (already committed, not re-run)

`perf/results/` carries the million-scale runs from M7: cold-start, op-calls 1M,
serve 1M (+saturation), webhook composed-path 1M, hour soak — all with machine + commit.
These predate this testing pass and were verified present, not re-executed.

## 8. Failures found

**None in the product.** Every contract in the guide held on first execution.

Process notes (not bugs): the guide's Linux-box verification block matches what was
observed here; `setInterval is not defined` appeared once inside the *example script*
(scope of `rt.*` is per-module) — expected surface, not a defect.

## 9. Verdict

jsrt passes the external testing guide end-to-end at `c30ac2e`: build, 84 tests,
all 8 exit codes, capability fences (fs/net/serve), supervision budgets, and the
webhook lifecycle drill. No fixes were required; the only change is the committed
testing guide plus refreshed drill artifacts.

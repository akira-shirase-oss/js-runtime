#!/usr/bin/env python3
"""M7 load harness — cold start x1000, op boundary x1M, rt.serve x1M.

The workspace load standard applied to the runtime itself. One JSON
artifact per scenario under perf/results/, tagged with commit + machine.
Misses and failures are written down in the artifact, never rerun away.
"""
import json
import os
import resource
import socket
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "release" / "jsrt"
LOAD = ROOT / "target" / "release" / "jsrt-load"
SCRIPTS = ROOT / "perf" / "scripts"
RESULTS = ROOT / "perf" / "results"

COLD_RUNS = 1000
COLD_WARMUP = 20
SERVE_PORT = 9410
SERVE_REQUESTS = 1_000_000
SERVE_CONCURRENCY = 32


def run_ms(args, **kw) -> tuple[float, subprocess.CompletedProcess]:
    t0 = time.perf_counter()
    proc = subprocess.run(args, capture_output=True, text=True, **kw)
    return (time.perf_counter() - t0) * 1000, proc


def pct(samples, q):
    ordered = sorted(samples)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))]


def summarize(samples):
    return {
        "n": len(samples),
        "min": round(min(samples), 3),
        "p50": round(pct(samples, 0.50), 3),
        "p95": round(pct(samples, 0.95), 3),
        "p99": round(pct(samples, 0.99), 3),
        "mean": round(statistics.fmean(samples), 3),
        "max": round(max(samples), 3),
    }


def machine() -> dict:
    def sysctl(key):
        return subprocess.run(["sysctl", "-n", key], capture_output=True, text=True).stdout.strip()

    commit = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True, cwd=ROOT
    ).stdout.strip()
    return {
        "commit": commit,
        "date_utc": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "arch": sysctl("hw.machine"),
        "chip": sysctl("machdep.cpu.brand_string"),
        "cores": int(sysctl("hw.ncpu")),
        "mem_bytes": int(sysctl("hw.memsize")),
        "os": subprocess.run(["sw_vers", "-productVersion"], capture_output=True, text=True).stdout.strip(),
        "build": "release (opt-level 3)",
    }


def write_result(name: str, payload: dict) -> Path:
    RESULTS.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H-%M-%SZ")
    out = RESULTS / f"{name}-{stamp}-{payload['environment']['commit']}.json"
    out.write_text(json.dumps(payload, indent=2) + "\n")
    print(f"wrote {out}")
    return out


def wait_port(port: int, proc: subprocess.Popen, timeout_s=15) -> None:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if proc.poll() is not None:
            sys.exit(f"serve process died early ({proc.returncode}): {proc.stderr.read()}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    proc.kill()
    sys.exit(f"port {port} never came up in {timeout_s}s")


def cold_start() -> None:
    for _ in range(COLD_WARMUP):
        run_ms([str(BIN), "run", str(SCRIPTS / "cold.js")])
    samples, fails = [], []
    for i in range(COLD_RUNS):
        ms, proc = run_ms([str(BIN), "run", str(SCRIPTS / "cold.js")])
        if proc.returncode == 0:
            samples.append(ms)
        else:
            fails.append({"i": i, "code": proc.returncode, "stderr": proc.stderr[:200]})
        if (i + 1) % 200 == 0:
            print(f"  cold {i + 1}/{COLD_RUNS}", file=sys.stderr)
    # ru_maxrss on Darwin is bytes; it is the max across ALL waited-for
    # children, i.e. the worst single spawn's peak.
    peak_rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    write_result("m7-1-cold-start", {
        "milestone": "M7.1",
        "workload": f"{COLD_RUNS} release-binary spawns of cold.js ({COLD_WARMUP} warmup), wall-clock spawn->exit",
        "environment": machine(),
        "cold_start_ms": summarize(samples) if samples else None,
        "failures": fails,
        "peak_child_rss_bytes": peak_rss,
        "caveats": ["spawn includes macOS exec+dyld, not just V8 init",
                    "peak_child_rss is the worst single child's max RSS (getrusage RUSAGE_CHILDREN)"],
    })


def op_million() -> None:
    ms, proc = run_ms([str(BIN), "run", str(SCRIPTS / "op-1m.js")])
    report = json.loads(proc.stdout.strip()) if proc.returncode == 0 else None
    write_result("m7-2a-op-1m", {
        "milestone": "M7.2a",
        "workload": "op-1m.js: 1,000,000 rt.now() calls, empty-loop baseline subtracted",
        "environment": machine(),
        "wall_ms": round(ms, 1),
        "exit_code": proc.returncode,
        "result": report,
        "stderr": proc.stderr[:500] if proc.returncode != 0 else None,
    })


def serve_million() -> None:
    proc = subprocess.Popen(
        [str(BIN), "run", str(SCRIPTS / "serve-load.js"),
         "--serve", f"127.0.0.1:{SERVE_PORT}", "--timeout", "0"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        wait_port(SERVE_PORT, proc)
        ms, lp = run_ms([
            str(LOAD), "--url", f"http://127.0.0.1:{SERVE_PORT}/",
            "--requests", str(SERVE_REQUESTS),
            "--concurrency", str(SERVE_CONCURRENCY),
        ])
        report = json.loads(lp.stdout.strip()) if lp.returncode == 0 else None
        load_stderr = lp.stderr[:500]
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
    write_result("m7-2b-serve-1m", {
        "milestone": "M7.2b",
        "workload": f"{SERVE_REQUESTS} GET / through rt.serve, {SERVE_CONCURRENCY} keep-alive connections (jsrt-load driver, client-observed latency)",
        "environment": machine(),
        "load_wall_ms": round(ms, 1),
        "driver": report,
        "driver_stderr": load_stderr if lp.returncode != 0 else None,
        "server_exit": proc.returncode,
        "caveats": [
            "latency is client-observed (includes client stack + loopback), not server service time",
            "single workstation; the driver shares cores with the server under test",
        ],
    })


def main() -> None:
    for b in (BIN, LOAD):
        if not b.exists():
            sys.exit(f"missing {b} — cargo build --release -p jsrt -p jsrt-load")
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "cold"):
        cold_start()
    if which in ("all", "op"):
        op_million()
    if which in ("all", "serve"):
        serve_million()


if __name__ == "__main__":
    main()

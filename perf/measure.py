#!/usr/bin/env python3
"""M1.3 measurement harness — cold start, op boundary, timers.

Measures the release binary end to end and writes one JSON artifact under
perf/results/ tagged with commit + machine so numbers are always read with
their context. These are workstation numbers, not production capacity.
"""
import json
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "release" / "jsrt"
SCRIPTS = ROOT / "perf" / "scripts"
RESULTS = ROOT / "perf" / "results"

COLD_RUNS = 100
COLD_WARMUP = 5
BENCH_REPS = 3


def run_ms(args: list[str]) -> tuple[float, str]:
    """Run a process, return (wall ms, stdout)."""
    t0 = time.perf_counter()
    proc = subprocess.run(args, capture_output=True, text=True)
    ms = (time.perf_counter() - t0) * 1000
    if proc.returncode != 0:
        sys.exit(f"bench run failed {args}: {proc.stderr.strip()}")
    return ms, proc.stdout


def pct(samples: list[float], q: float) -> float:
    ordered = sorted(samples)
    idx = min(len(ordered) - 1, int(q * len(ordered)))
    return ordered[idx]


def summarize(samples: list[float]) -> dict:
    return {
        "n": len(samples),
        "min": round(min(samples), 3),
        "p50": round(pct(samples, 0.50), 3),
        "p95": round(pct(samples, 0.95), 3),
        "mean": round(statistics.fmean(samples), 3),
        "max": round(max(samples), 3),
    }


def bench_json(name: str, reps: int) -> dict:
    """Run a bench script `reps` times, take the median of each metric."""
    runs = []
    for _ in range(reps):
        _, out = run_ms([str(BIN), "run", str(SCRIPTS / name)])
        runs.append(json.loads(out.strip()))
    keys = [k for k, v in runs[0].items() if isinstance(v, (int, float))]
    return {k: statistics.median(r[k] for r in runs) for k in keys}


def machine() -> dict:
    def sysctl(key: str) -> str:
        out = subprocess.run(["sysctl", "-n", key], capture_output=True, text=True)
        return out.stdout.strip()

    commit = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        capture_output=True, text=True, cwd=ROOT,
    ).stdout.strip()
    return {
        "commit": commit,
        "date_utc": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "arch": sysctl("hw.machine"),
        "chip": sysctl("machdep.cpu.brand_string"),
        "cores": int(sysctl("hw.ncpu")),
        "mem_bytes": int(sysctl("hw.memsize")),
        "os": subprocess.run(["sw_vers", "-productVersion"], capture_output=True, text=True).stdout.strip(),
        "build": "release (opt-level 3, thin lto default)",
    }


def main() -> None:
    if not BIN.exists():
        sys.exit(f"release binary missing: {BIN} — run `cargo build --release` first")

    # Cold start: full process spawn → parse → eval → op → exit.
    for _ in range(COLD_WARMUP):
        run_ms([str(BIN), "run", str(SCRIPTS / "cold.js")])
    cold = [run_ms([str(BIN), "run", str(SCRIPTS / "cold.js")])[0] for _ in range(COLD_RUNS)]

    result = {
        "milestone": "M1.3",
        "workload": {
            "cold_start": f"{COLD_RUNS} runs of cold.js ({COLD_WARMUP} warmup, wall-clock process spawn to exit)",
            "op_boundary": f"op-boundary.js x{BENCH_REPS}, median; 10k rt.now + 10k rt.hostInfo, empty-loop baseline subtracted",
            "timers": f"timers.js x{BENCH_REPS}, median; 500 sleep(0) + 100 sleep(1) sequential",
        },
        "environment": machine(),
        "cold_start_ms": summarize(cold),
        "op_boundary": bench_json("op-boundary.js", BENCH_REPS),
        "timers": bench_json("timers.js", BENCH_REPS),
        "caveats": [
            "single workstation, one release build, no contention",
            "cold start includes macOS process spawn + dyld, not just V8 init",
            "boundary_ns numbers subtract a JS loop baseline — approximation, not isolation",
        ],
    }

    RESULTS.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H-%M-%SZ")
    out = RESULTS / f"m1-3-{stamp}-{result['environment']['commit']}.json"
    out.write_text(json.dumps(result, indent=2) + "\n")
    print(f"wrote {out}")
    print(json.dumps({k: result[k] for k in ("cold_start_ms", "op_boundary", "timers")}, indent=2))


if __name__ == "__main__":
    main()

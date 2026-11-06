#!/usr/bin/env python3
"""M7.3 soak driver — one hour of timers/fetch churn, RSS + retained-work drift.

Spawns the serve target and the soak script, samples both processes' RSS
once a minute, collects the script's own pending-timer reports, and at
the end classifies drift honestly: growth measured, not smoothed over.
"""
import json
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "release" / "jsrt"
SCRIPTS = ROOT / "perf" / "scripts"
RESULTS = ROOT / "perf" / "results"

DURATION_S = int(sys.argv[2]) if len(sys.argv) > 2 else 3600
SAMPLE_EVERY_S = 60
SERVE_PORT = 9410


def sysctl(key):
    return subprocess.run(["sysctl", "-n", key], capture_output=True, text=True).stdout.strip()


def rss_bytes(pid: int) -> int | None:
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                         capture_output=True, text=True).stdout.strip()
    try:
        return int(out) * 1024  # ps reports KiB
    except ValueError:
        return None


def main() -> None:
    commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"],
                            capture_output=True, text=True, cwd=ROOT).stdout.strip()
    if not BIN.exists():
        sys.exit("release binary missing — cargo build --release -p jsrt")

    serve = subprocess.Popen(
        [str(BIN), "run", str(SCRIPTS / "serve-load.js"),
         "--serve", f"127.0.0.1:{SERVE_PORT}", "--timeout", "0"],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    try:
        deadline = time.time() + 15
        import socket
        while time.time() < deadline:
            if serve.poll() is not None:
                sys.exit(f"serve target died: {serve.stderr.read()}")
            try:
                with socket.create_connection(("127.0.0.1", SERVE_PORT), 0.2):
                    break
            except OSError:
                time.sleep(0.05)

        soak = subprocess.Popen(
            [str(BIN), "run", str(SCRIPTS / "soak.js"),
             "--net", f"127.0.0.1:{SERVE_PORT}", "--timeout", "0"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

        status_lines = []
        def read_status():
            for line in soak.stdout:
                line = line.strip()
                if line.startswith("{"):
                    status_lines.append(line)
        reader = threading.Thread(target=read_status, daemon=True)
        reader.start()

        t0 = time.time()
        rss_series = []
        while time.time() - t0 < DURATION_S:
            time.sleep(SAMPLE_EVERY_S)
            rss_series.append({
                "t_s": int(time.time() - t0),
                "soak_rss": rss_bytes(soak.pid),
                "serve_rss": rss_bytes(serve.pid),
                "soak_alive": soak.poll() is None,
            })
            if soak.poll() is not None:
                break
        soak.terminate()
        try:
            soak.wait(timeout=10)
        except subprocess.TimeoutExpired:
            soak.kill()
    finally:
        serve.terminate()
        try:
            serve.wait(timeout=10)
        except subprocess.TimeoutExpired:
            serve.kill()

    reports = [json.loads(l) for l in status_lines]
    first_rss = next((s["soak_rss"] for s in rss_series if s["soak_rss"]), None)
    last_rss = next((s["soak_rss"] for s in reversed(rss_series) if s["soak_rss"]), None)
    drift = (last_rss - first_rss) if first_rss and last_rss else None

    result = {
        "milestone": "M7.3",
        "workload": f"soak.js for {DURATION_S}s: setTimeout(0) churn + self-cancelling intervals + loopback fetch every 4th iter; RSS sampled every {SAMPLE_EVERY_S}s",
        "environment": {
            "commit": commit,
            "date_utc": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "arch": sysctl("hw.machine"), "chip": sysctl("machdep.cpu.brand_string"),
            "cores": int(sysctl("hw.ncpu")), "mem_bytes": int(sysctl("hw.memsize")),
            "os": subprocess.run(["sw_vers", "-productVersion"], capture_output=True, text=True).stdout.strip(),
            "build": "release (opt-level 3)",
        },
        "duration_s": int(time.time() - t0),
        "rss_series": rss_series,
        "soak_rss_first_bytes": first_rss,
        "soak_rss_last_bytes": last_rss,
        "soak_rss_drift_bytes": drift,
        "status_reports": reports,
        "last_status": reports[-1] if reports else None,
        "soak_exit": soak.returncode,
        "caveats": [
            "RSS sampled via ps once/min — coarse, drift is the signal not the level",
            "interval churn reschedules continuously; a cancel-path leak shows as pending growth in last_status",
        ],
    }
    RESULTS.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H-%M-%SZ")
    out = RESULTS / f"m7-3-soak-{stamp}-{commit}.json"
    out.write_text(json.dumps(result, indent=2) + "\n")
    print(f"wrote {out}")
    print(json.dumps({k: result[k] for k in
                      ("soak_rss_first_bytes", "soak_rss_last_bytes",
                       "soak_rss_drift_bytes", "last_status", "soak_exit")}, indent=2))


if __name__ == "__main__":
    main()

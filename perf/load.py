#!/usr/bin/env python3
"""M7 load harness — cold start x1000, op boundary x1M, rt.serve x1M,
the 128-connection saturation probe and the composed webhook app x1M.

The workspace load standard applied to the runtime itself. One JSON
artifact per scenario under perf/results/, tagged with commit + machine.
Misses and failures are written down in the artifact, never rerun away.
"""
import json
import os
import platform
import resource
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "release" / "jsrt"
LOAD = ROOT / "target" / "release" / "jsrt-load"
SCRIPTS = ROOT / "perf" / "scripts"
RESULTS = ROOT / "perf" / "results"

COLD_RUNS = 1000
COLD_WARMUP = 20
# jsrt's port band on the maintainer laptop is 47400-47499 (other projects
# own the neighbouring bands). serve-load.js and soak.js carry the same
# literal: a script's rt.serve address must equal its grant.
SERVE_PORT = 47410
WEBHOOK_PORT = 47411  # examples/webhook's own port
SERVE_REQUESTS = 1_000_000
SERVE_CONCURRENCY = 32
SATURATION_CONCURRENCY = 128
WEBHOOK = ROOT / "examples" / "webhook"
# The four LOAD_MATRIX gap scenarios. Full scale is the committed
# default; JSRT_LOAD_UNITS scales every unit down for a local pass.
UNITS = int(os.environ.get("JSRT_LOAD_UNITS", "0")) or None
KV_UNITS = UNITS or 1_000_000
MEDIA_UNITS = UNITS or 100_000
MEDIA_FILES = 64
REFUSE_UNITS = UNITS or 1_000_000
CHURN_ROUNDS = UNITS or 1_000
CHURN_PORT = 47412


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


def loadavg() -> list[float]:
    """1/5/15-minute load averages — a busy machine is named in the result."""
    return [round(x, 2) for x in os.getloadavg()]


def machine() -> dict:
    def sysctl(key):
        return subprocess.run(["sysctl", "-n", key], capture_output=True, text=True).stdout.strip()

    def linux_chip() -> str:
        try:
            for line in Path("/proc/cpuinfo").read_text().splitlines():
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
        except OSError:
            pass
        return platform.processor() or "unknown"

    def mem_bytes() -> int:
        if platform.system() == "Darwin":
            raw = sysctl("hw.memsize")
            if raw.isdigit():
                return int(raw)
            try:
                return os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGESIZE")
            except (ValueError, OSError):
                return 0
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                return int(line.split()[1]) * 1024
        return 0

    commit = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True, cwd=ROOT
    ).stdout.strip()
    if platform.system() == "Darwin":
        u = platform.uname()
        arch = sysctl("hw.machine") or u.machine
        chip = sysctl("machdep.cpu.brand_string") or u.processor or "unknown"
        os_name = (
            subprocess.run(
                ["sw_vers", "-productVersion"], capture_output=True, text=True
            ).stdout.strip()
            or u.version
        )
    else:
        u = platform.uname()
        arch = u.machine
        chip = linux_chip()
        os_name = f"{u.system} {u.release}"
    return {
        "commit": commit,
        "date_utc": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "arch": arch,
        "chip": chip,
        "cores": os.cpu_count() or 0,
        "mem_bytes": mem_bytes(),
        "os": os_name,
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
    load_before = loadavg()
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
        "loadavg_before": load_before,
        "loadavg_after": loadavg(),
        "caveats": ["spawn includes macOS exec+dyld, not just V8 init",
                    "peak_child_rss is the worst single child's max RSS (getrusage RUSAGE_CHILDREN)"],
    })


def op_million() -> None:
    load_before = loadavg()
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
        "loadavg_before": load_before,
        "loadavg_after": loadavg(),
    })


def serve_million(concurrency: int = SERVE_CONCURRENCY,
                  name: str = "m7-2b-serve-1m", milestone: str = "M7.2b") -> None:
    load_before = loadavg()
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
            "--concurrency", str(concurrency),
        ])
        report = json.loads(lp.stdout.strip()) if lp.returncode == 0 else None
        load_stderr = lp.stderr[:500]
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
    write_result(name, {
        "milestone": milestone,
        "workload": f"{SERVE_REQUESTS} GET / through rt.serve, {concurrency} keep-alive connections (jsrt-load driver, client-observed latency)",
        "environment": machine(),
        "load_wall_ms": round(ms, 1),
        "driver": report,
        "driver_stderr": load_stderr if lp.returncode != 0 else None,
        "server_exit": proc.returncode,
        "loadavg_before": load_before,
        "loadavg_after": loadavg(),
        "caveats": [
            "latency is client-observed (includes client stack + loopback), not server service time",
            "single workstation; the driver shares cores with the server under test",
        ],
    })


def saturation_million() -> None:
    serve_million(SATURATION_CONCURRENCY, "m7-2b-serve-1m-saturation", "M7.2b-saturation")


def http(method: str, url: str, headers: dict | None = None, body: bytes | None = None):
    req = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    with urllib.request.urlopen(req, timeout=5) as r:
        return r.status, r.read().decode()


def webhook_million() -> None:
    """The real webhook app (examples/webhook) on the dedup path at 1M.

    It runs from a temporary copy (app.js and its manifest, unchanged) so
    the example's own data is not touched. One POST seeds delivery d1, the
    million repeat it, and the stored key list is read back afterwards:
    dedup held only if it is still exactly the one key.
    """
    load_before = loadavg()
    base = f"http://127.0.0.1:{WEBHOOK_PORT}"
    with tempfile.TemporaryDirectory(prefix="jsrt-webhook-") as tmp:
        work = Path(tmp)
        (work / "data").mkdir()
        for f in ("app.js", "jsrt.manifest.json"):
            (work / f).write_text((WEBHOOK / f).read_text())
        proc = subprocess.Popen(
            [str(BIN), "run", str(work / "app.js"),
             "--manifest", str(work / "jsrt.manifest.json"), "--timeout", "0"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        try:
            wait_port(WEBHOOK_PORT, proc)
            seed = http("POST", f"{base}/hooks/github", {"X-Delivery-Id": "d1"}, b'{"a":1}')
            ms, lp = run_ms([
                str(LOAD), "--url", f"{base}/hooks/github",
                "--requests", str(SERVE_REQUESTS),
                "--concurrency", str(SERVE_CONCURRENCY),
                "--method", "POST", "--header", "X-Delivery-Id: d1", "--body", '{"a":1}',
            ])
            report = json.loads(lp.stdout.strip()) if lp.returncode == 0 else None
            load_stderr = lp.stderr[:500]
            _, listed = http("GET", f"{base}/hooks/github")
            keys_after = json.loads(listed)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
    write_result("m7-2b-webhook-1m-composed", {
        "milestone": "M7.2b-composed",
        "workload": f"{SERVE_REQUESTS} POST /hooks/github X-Delivery-Id:d1 through the real webhook app (examples/webhook on port {WEBHOOK_PORT}, temporary copy) - the dedup path: serve dispatch + rt.kv.get on a live store. {SERVE_CONCURRENCY} keep-alive connections.",
        "environment": machine(),
        "load_wall_ms": round(ms, 1),
        "seed": {"status": seed[0], "body": seed[1]},
        "driver": report,
        "driver_stderr": load_stderr if lp.returncode != 0 else None,
        "keys_after": keys_after,
        "dedup_held": keys_after == ["hook/github/d1"],
        "server_exit": proc.returncode,
        "loadavg_before": load_before,
        "loadavg_after": loadavg(),
        "caveats": [
            "latency is client-observed (includes client stack + loopback), not server service time",
            "single workstation; the driver shares cores with the server under test",
        ],
    })


def materialize(template: str, dst: Path, subs: dict) -> Path:
    """Render a perf/scripts template (__NAME__ placeholders) into dst."""
    text = (SCRIPTS / template).read_text()
    for k, v in subs.items():
        text = text.replace(f"__{k}__", str(v))
    dst.write_text(text)
    return dst


def kv_sustained() -> None:
    """N mixed kv ops on a granted store, then writes until the quota
    refuses — integrity, quota enforcement and store sanity under load."""
    load_before = loadavg()
    with tempfile.TemporaryDirectory(prefix="jsrt-kv-") as tmp:
        work = Path(tmp)
        store = work / "load.kv"
        script = materialize("kv-sustained.js", work / "kv-sustained.js", {"UNITS": KV_UNITS})
        quota_mb = 8
        ms, proc = run_ms([
            str(BIN), "run", str(script),
            "--kv", str(store), "--kv-max-mb", str(quota_mb), "--timeout", "0",
        ])
        report = json.loads(proc.stdout.strip().splitlines()[-1]) if proc.returncode == 0 else None
        store_bytes = store.stat().st_size if store.exists() else 0
    thresholds = {
        "ops": KV_UNITS,
        "integrity_fail": 0,
        "quota_refusals_min": 1,
        "refusals_all_capability_named": True,
        "post_quota_ok": True,
        "store_bytes_max": quota_mb * 1024 * 1024 + 1024 * 1024,  # quota + one write + sqlite pages
        "exit_code": 0,
    }
    checks = {
        "ops": report is not None and report["ops"] == KV_UNITS,
        "integrity_fail": report is not None and report["integrity_fail"] == 0,
        "quota_refusals_min": report is not None and report["quota_refusals"] >= 1,
        "refusals_all_capability_named": report is not None
        and report["refusals_named_capability"] == report["quota_refusals"],
        "post_quota_ok": report is not None and report["post_quota_ok"] is True,
        "store_bytes_max": store_bytes <= thresholds["store_bytes_max"],
        "exit_code": proc.returncode == 0,
    }
    write_result("m8-1-kv-sustained", {
        "milestone": "M8.1",
        "workload": f"{KV_UNITS} mixed rt.kv get/put/list/delete ops on a granted store (1024-key working set, kv-sustained.js), then 64 KiB writes until the {quota_mb} MiB quota refuses",
        "environment": machine(),
        "wall_ms": round(ms, 1),
        "result": report,
        "store_bytes": store_bytes,
        "thresholds": thresholds,
        "checks": checks,
        "passed": all(checks.values()),
        "stderr": proc.stderr[:500] if proc.returncode != 0 else None,
        "loadavg_before": load_before,
        "loadavg_after": loadavg(),
    })


def media_sustained() -> None:
    """N streamed rt.media.put from a read-granted dir, dedup every 10th,
    stat on every distinct name — the store must hold exactly the source
    set, complete files only."""
    load_before = loadavg()
    with tempfile.TemporaryDirectory(prefix="jsrt-media-") as tmp:
        work = Path(tmp)
        src, store = work / "src", work / "store"
        src.mkdir()
        store.mkdir()
        # Distinct deterministic content per file — dedup is real work,
        # not one repeated hash.
        src_bytes = 0
        for i in range(MEDIA_FILES):
            body = (f"media-source-{i}-" + "y" * (4096 + i * 64)).encode()
            (src / f"src-{i}.bin").write_bytes(body)
            src_bytes += len(body)
        script = materialize(
            "media-sustained.js",
            work / "media-sustained.js",
            {"UNITS": MEDIA_UNITS, "SRC": src, "FILES": MEDIA_FILES},
        )
        ms, proc = run_ms([
            str(BIN), "run", str(script),
            "--media-root", str(store), "--read", str(src), "--timeout", "0",
        ])
        report = json.loads(proc.stdout.strip().splitlines()[-1]) if proc.returncode == 0 else None
        stored = [p for p in store.iterdir() if p.is_file()] if store.exists() else []
        stored_tmp = [p for p in stored if ".tmp" in p.name or p.name.startswith("tmp")]
        stored_bytes = sum(p.stat().st_size for p in stored)
    thresholds = {
        "puts": MEDIA_UNITS,
        "distinct_names": MEDIA_FILES,
        "stored_files": MEDIA_FILES,
        "stored_tmp_files": 0,
        "stored_bytes": src_bytes,
        "stat_ok": MEDIA_FILES,
        "exit_code": 0,
    }
    checks = {
        "puts": report is not None and report["puts"] == MEDIA_UNITS,
        "distinct_names": report is not None and report["distinct_names"] == MEDIA_FILES,
        "stored_files": len(stored) == MEDIA_FILES,
        "stored_tmp_files": len(stored_tmp) == 0,
        "stored_bytes": stored_bytes == src_bytes,
        "stat_ok": report is not None and report["stat_ok"] == MEDIA_FILES,
        "exit_code": proc.returncode == 0,
    }
    write_result("m8-2-media-sustained", {
        "milestone": "M8.2",
        "workload": f"{MEDIA_UNITS} rt.media.put over {MEDIA_FILES} distinct sources (~4-8 KiB each), dedup every 10th op, stat per distinct name",
        "environment": machine(),
        "wall_ms": round(ms, 1),
        "result": report,
        "stored_files": len(stored),
        "stored_bytes": stored_bytes,
        "thresholds": thresholds,
        "checks": checks,
        "passed": all(checks.values()),
        "stderr": proc.stderr[:500] if proc.returncode != 0 else None,
        "loadavg_before": load_before,
        "loadavg_after": loadavg(),
    })


def refuse_flood() -> None:
    """N ungated op calls in one run: every refusal must classify as a
    named CapabilityError, the run must end clean, nothing may leak."""
    load_before = loadavg()
    with tempfile.TemporaryDirectory(prefix="jsrt-refuse-") as tmp:
        work = Path(tmp)
        script = materialize("refuse.js", work / "refuse.js", {"UNITS": REFUSE_UNITS})
        ms, proc = run_ms([str(BIN), "run", str(script), "--timeout", "0"])
        report = json.loads(proc.stdout.strip().splitlines()[-1]) if proc.returncode == 0 else None
    thresholds = {
        "attempts": REFUSE_UNITS,
        "refused": REFUSE_UNITS,
        "capability_named": REFUSE_UNITS,
        "not_refused": 0,
        "exit_code": 0,
    }
    checks = {
        "attempts": report is not None and report["attempts"] == REFUSE_UNITS,
        "refused": report is not None and report["refused"] == REFUSE_UNITS,
        "capability_named": report is not None and report["capability_named"] == REFUSE_UNITS,
        "not_refused": report is not None and report["not_refused"] == 0,
        "exit_code": proc.returncode == 0,
    }
    write_result("m8-3-refusal-flood", {
        "milestone": "M8.3",
        "workload": f"{REFUSE_UNITS} ungated op calls (rt.kv/rt.fetch/rt.media/rt.readText) in one ungranted run — refusals must classify fast and clean",
        "environment": machine(),
        "wall_ms": round(ms, 1),
        "result": report,
        "thresholds": thresholds,
        "checks": checks,
        "passed": all(checks.values()),
        "stderr": proc.stderr[:500] if proc.returncode != 0 else None,
        "loadavg_before": load_before,
        "loadavg_after": loadavg(),
    })


def supervise_churn() -> None:
    """N spawn->ready->SIGTERM cycles: the worker holds a port, so an
    orphaned worker breaks the next respawn. Every kill must classify."""
    load_before = loadavg()
    rounds_done = 0
    exits: dict[int, int] = {}
    orphans = 0
    respawn_failed = 0
    t0 = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix="jsrt-churn-") as tmp:
        report_path = Path(tmp) / "runs.jsonl"
        for i in range(CHURN_ROUNDS):
            proc = subprocess.Popen(
                [str(BIN), "supervise", str(SCRIPTS / "churn-worker.js"),
                 "--serve", f"127.0.0.1:{CHURN_PORT}",
                 "--report", str(report_path), "--wall-ms", "30000"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
            try:
                wait_port(CHURN_PORT, proc, timeout_s=10)
            except SystemExit:
                respawn_failed += 1
                proc.kill()
                break
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                orphans += 1
                proc.kill()
            exits[proc.returncode] = exits.get(proc.returncode, 0) + 1
            rounds_done += 1
            if (i + 1) % 200 == 0:
                print(f"  churn {i + 1}/{CHURN_ROUNDS}", file=sys.stderr)
        # The port must be free after the last kill — an orphan holds it.
        try:
            with socket.create_connection(("127.0.0.1", CHURN_PORT), timeout=0.2):
                orphans += 1
        except OSError:
            pass
        records = []
        if report_path.exists():
            records = [
                json.loads(line)
                for line in report_path.read_text().splitlines()
                if line.strip() and '"run_id"' in line
            ]
    wall_ms = (time.perf_counter() - t0) * 1000
    classified = sum(1 for r in records if r.get("killed_by") == "operator-sigterm")
    thresholds = {
        "rounds": CHURN_ROUNDS,
        "exit_code": 6,
        "killed_by": "operator-sigterm",
        "respawn_failed": 0,
        "orphans": 0,
    }
    checks = {
        "rounds": rounds_done == CHURN_ROUNDS,
        "exit_code": set(exits) <= {6},
        "killed_by": classified == rounds_done,
        "respawn_failed": respawn_failed == 0,
        "orphans": orphans == 0,
    }
    write_result("m8-4-supervise-churn", {
        "milestone": "M8.4",
        "workload": f"{CHURN_ROUNDS} jsrt supervise spawn->ready->SIGTERM cycles of a serve worker on port {CHURN_PORT} — an orphaned worker would hold the port and break the next respawn",
        "environment": machine(),
        "wall_ms": round(wall_ms, 1),
        "rounds": rounds_done,
        "exit_codes": exits,
        "records_classified_operator_sigterm": classified,
        "report_records": len(records),
        "respawn_failed": respawn_failed,
        "orphans": orphans,
        "thresholds": thresholds,
        "checks": checks,
        "passed": all(checks.values()),
        "loadavg_before": load_before,
        "loadavg_after": loadavg(),
        "caveats": ["port-free-per-round is the orphan proof; process listing is not used"],
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
    if which in ("all", "saturation"):
        saturation_million()
    if which in ("all", "webhook"):
        webhook_million()
    if which in ("all", "kv"):
        kv_sustained()
    if which in ("all", "media"):
        media_sustained()
    if which in ("all", "refuse"):
        refuse_flood()
    if which in ("all", "churn"):
        supervise_churn()


if __name__ == "__main__":
    main()

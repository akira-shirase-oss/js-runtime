//! `jsrt supervise` — the worker-process boundary, proven end to end.
//!
//! The in-process watchdog can stop JavaScript; it cannot stop allocation
//! and it cannot outlive the isolate it watches. These tests spawn the real
//! binary supervising the real binary — budgets enforced by the kernel
//! (rlimits) or by footprint polling, never by a flag inside the runtime.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const JSRT: &str = env!("CARGO_BIN_EXE_jsrt");

struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}")),
        }
    }

    fn script(&self, name: &str, source: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, source).unwrap_or_else(|e| panic!("write: {e}"));
        path
    }

    fn supervise(&self, script: &Path, extra: &[&str]) -> Output {
        let mut args = vec!["supervise"];
        args.push(script.to_str().unwrap_or_else(|| panic!("utf8")));
        args.extend_from_slice(extra);
        Command::new(JSRT)
            .args(&args)
            .current_dir(self.dir.path())
            .output()
            .unwrap_or_else(|e| panic!("spawn jsrt: {e}"))
    }
}

fn code(out: &Output) -> i32 {
    out.status
        .code()
        .unwrap_or_else(|| panic!("killed by signal — status: {:?}", out.status))
}

/// The last stdout line is the JSON run report (the worker's own prints
/// come first). Parse it and hand back the object.
fn report(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("no report in stdout: {stdout}"));
    serde_json::from_str(line).unwrap_or_else(|e| panic!("report parse: {e} — {line}"))
}

#[test]
fn a_clean_supervised_run_reports_clean() {
    let rig = Rig::new();
    let script = rig.script("ok.js", "rt.print('from the worker\\n');\n");
    let out = rig.supervise(&script, &[]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = report(&out);
    assert_eq!(r["outcome"], "clean");
    assert_eq!(r["exit_code"], 0);
    assert!(r["peak_rss_bytes"].as_u64().unwrap_or(0) > 0, "{r}");
    assert!(r["run_id"].as_str().is_some_and(|s| !s.is_empty()), "{r}");
}

#[test]
fn the_workers_exit_class_passes_through() {
    let rig = Rig::new();
    // 5 (exception) and 7 (capability) must survive the process boundary —
    // the supervisor classifies, never rewrites.
    let boom = rig.script("boom.js", "throw new Error('nope');\n");
    let deny = rig.script("deny.js", "await rt.readText('/etc/passwd');\n");
    let out = rig.supervise(&boom, &[]);
    assert_eq!(code(&out), 5);
    assert_eq!(report(&out)["outcome"], "exception");
    let out = rig.supervise(&deny, &[]);
    assert_eq!(code(&out), 7);
    assert_eq!(report(&out)["outcome"], "capability");
}

#[test]
fn the_cpu_budget_is_enforced_by_the_kernel() {
    let rig = Rig::new();
    let script = rig.script("spin.js", "while (true) {}\n");
    // The worker's own watchdog would also fire (at 60s) — the rlimit must
    // get there first, at 1 CPU-second.
    let out = rig.supervise(&script, &["--timeout", "60000", "--max-cpu", "1"]);
    assert_eq!(
        code(&out),
        6,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = report(&out);
    assert_eq!(r["outcome"], "terminated");
    assert_eq!(r["killed_by"], "cpu-budget");
    assert_eq!(r["signal"], 24, "{r}"); // SIGXCPU
}

#[test]
fn the_rss_budget_kills_a_memory_bomb() {
    let rig = Rig::new();
    // 8 MiB chunks in a loop — the footprint climbs past the budget while
    // the in-process watchdog can only watch.
    let script = rig.script(
        "bomb.js",
        "const c = [];\nfor (;;) c.push(new Uint8Array(8 * 1024 * 1024));\n",
    );
    let out = rig.supervise(
        &script,
        &[
            "--timeout",
            "60000",
            "--max-rss-mb",
            "200",
            "--kill-grace-ms",
            "200",
        ],
    );
    assert_eq!(
        code(&out),
        6,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = report(&out);
    assert_eq!(r["outcome"], "terminated");
    assert_eq!(r["killed_by"], "rss-budget");
    // The peak should be near the budget, not unbounded — the bound is the
    // poll interval, not the bomb's appetite.
    let peak = r["peak_rss_bytes"].as_u64().unwrap_or(u64::MAX);
    assert!(
        peak < 1024 * 1024 * 1024,
        "peak {peak} — the bomb was not bounded"
    );
}

#[test]
fn the_wall_backstop_kills_a_stalled_worker() {
    let rig = Rig::new();
    // A promise that never resolves: the worker's own watchdog is the
    // honest first line, but the supervisor's wall covers every path —
    // including the ones the watchdog can't see.
    let script = rig.script("stall.js", "await new Promise(() => {});\n");
    let out = rig.supervise(&script, &["--timeout", "60000", "--wall-ms", "800"]);
    assert_eq!(
        code(&out),
        6,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = report(&out);
    assert_eq!(r["killed_by"], "wall-clock");
}

#[test]
fn a_report_file_collects_jsonl_records() {
    let rig = Rig::new();
    let script = rig.script("ok.js", "rt.print('x\\n');\n");
    let log = rig.dir.path().join("runs.jsonl");
    let log_s = log.to_str().unwrap_or_else(|| panic!("utf8")).to_string();
    let out = rig.supervise(&script, &["--report", &log_s]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let content = fs::read_to_string(&log).unwrap_or_else(|e| panic!("read log: {e}"));
    let r: Value =
        serde_json::from_str(content.trim()).unwrap_or_else(|e| panic!("log parse: {e}"));
    assert_eq!(r["outcome"], "clean");
}

#[test]
fn a_flooded_log_rotates_and_counts_its_drops() {
    let rig = Rig::new();
    let script = rig.script("ok.js", "rt.print('x\\n');\n");
    let log = rig.dir.path().join("runs.jsonl");
    let log_s = log.to_str().unwrap_or_else(|| panic!("utf8")).to_string();
    // A 1 KiB cap with ~600-byte run records rotates every other write.
    for _ in 0..8 {
        let out = rig.supervise(&script, &["--report", &log_s, "--report-max-kb", "1"]);
        assert_eq!(
            code(&out),
            0,
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // The rotated generation exists — the log stayed bounded.
    let rotated = rig.dir.path().join("runs.jsonl.1");
    assert!(rotated.exists(), "no rotation happened");
    let live = fs::read_to_string(&log).unwrap_or_else(|e| panic!("read log: {e}"));
    // The first record of the current generation is the rotation meta —
    // and it must say how many records it displaced.
    let first: Value =
        serde_json::from_str(live.lines().next().unwrap_or_else(|| panic!("empty log")))
            .unwrap_or_else(|e| panic!("parse: {e}"));
    assert_eq!(first["type"], "rotation", "{first}");
    assert!(
        first["dropped_records"].as_u64().unwrap_or(0) > 0,
        "dropped count not visible: {first}"
    );
}

#[test]
fn an_operator_sigterm_reaches_the_worker() {
    // The drill found this hole: kill the supervisor, the worker used to
    // stay alive holding its port. Now SIGTERM forwards through the same
    // TERM->KILL path the budgets use.
    let rig = Rig::new();
    let script = rig.script("stall.js", "await new Promise(() => {});\n");
    let mut child = Command::new(JSRT)
        .args([
            "supervise",
            script.to_str().unwrap_or_else(|| panic!("utf8")),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn supervise: {e}"));

    // Signal on readiness, not a timer: the supervisor prints its armed
    // line once the worker exists and the handlers are installed. Killing
    // on a fixed sleep is a race — a slow exec can put the signal inside
    // the supervisor's own startup window, which is exactly what this
    // test exists to keep out of production.
    let stderr = child
        .stderr
        .take()
        .unwrap_or_else(|| panic!("no stderr pipe"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut worker_pid = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while worker_pid.is_none() {
        let left = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_else(|| panic!("supervisor never armed: no readiness line in 15s"));
        let line = rx
            .recv_timeout(left)
            .unwrap_or_else(|e| panic!("supervisor stderr closed before arming: {e}"));
        if let Some(rest) = line.strip_prefix("jsrt: supervise: worker ") {
            worker_pid = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<i32>().ok());
        }
    }
    let worker_pid = worker_pid.unwrap_or_else(|| panic!("no pid in readiness line"));

    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .output()
        .unwrap_or_else(|e| panic!("kill: {e}"));
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    assert_eq!(
        code(&out),
        6,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = report(&out);
    assert_eq!(r["outcome"], "terminated", "{r}");
    assert_eq!(r["killed_by"], "operator-sigterm", "{r}");
    assert_eq!(r["signal"], 15, "{r}");

    // The actual hole: the worker must be gone, not just reported dead —
    // a live worker is what held the port in the drill.
    let status = Command::new("kill")
        .args(["-0", &worker_pid.to_string()])
        .output()
        .unwrap_or_else(|e| panic!("kill -0: {e}"))
        .status;
    assert!(
        !status.success(),
        "worker {worker_pid} survived the supervisor's shutdown"
    );
}

#[test]
fn a_flooding_worker_is_capped_per_stream_and_the_drop_is_counted() {
    let rig = Rig::new();
    // 200 KiB to stdout, then an uncaught error whose message is 200 KiB —
    // the host's own stderr line — against a 16 KiB cap per stream.
    let script = rig.script(
        "flood.js",
        "const line = 'x'.repeat(1023) + '\\n';\n\
         for (let i = 0; i < 200; i++) rt.print(line);\n\
         throw new Error('y'.repeat(200 * 1024));\n",
    );
    let out = rig.supervise(&script, &["--max-output-kb", "16"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        code(&out),
        5,
        "stderr tail: {}",
        &stderr[stderr.len().saturating_sub(400)..]
    );
    let r = report(&out);
    let cap: u64 = 16 * 1024;
    assert_eq!(r["output"]["max_bytes_per_stream"], cap, "{r}");
    assert_eq!(r["output"]["stdout_bytes"], cap, "{r}");
    assert_eq!(r["output"]["stdout_dropped_bytes"], 200 * 1024 - cap, "{r}");
    assert_eq!(r["output"]["stderr_bytes"], cap, "{r}");
    let stderr_dropped = r["output"]["stderr_dropped_bytes"].as_u64().unwrap_or(0);
    assert!(stderr_dropped >= 200 * 1024 - cap, "{r}");
    // What reached the operator is bounded: the capped worker output, the
    // re-shown tail (at most 4 KiB plus a marker line) and the
    // supervisor's own lines (readiness, drop notices, the report).
    let tail = 4096 + 200;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let worker_stdout: u64 = stdout
        .lines()
        .filter(|l| !l.starts_with('{'))
        .map(|l| l.len() as u64 + 1)
        .sum();
    assert!(worker_stdout <= cap + tail, "{worker_stdout} bytes");
    assert!(
        stderr.contains("worker stdout reached the 16384-byte cap")
            && stderr.contains("worker stderr reached the 16384-byte cap"),
        "stderr tail: {}",
        &stderr[stderr.len().saturating_sub(400)..]
    );
    assert!(
        (stderr.len() as u64) < cap + tail + 2048,
        "{} stderr bytes",
        stderr.len()
    );
}

#[test]
fn the_cause_of_death_survives_a_flooded_stream() {
    let rig = Rig::new();
    // The host's own final line is 200 KiB long and ends with the part an
    // operator needs; the stream cap is 16 KiB.
    let script = rig.script(
        "die.js",
        "throw new Error('y'.repeat(200 * 1024) + ' THE-REAL-CAUSE-OF-DEATH');\n",
    );
    let out = rig.supervise(&script, &["--max-output-kb", "16"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        code(&out),
        5,
        "stderr tail: {}",
        &stderr[stderr.len().saturating_sub(300)..]
    );
    assert!(
        stderr.contains("THE-REAL-CAUSE-OF-DEATH"),
        "the end of the worker's stderr was dropped: {}",
        &stderr[stderr.len().saturating_sub(300)..]
    );
    let r = report(&out);
    assert!(
        r["output"]["stderr_tail_bytes"].as_u64().unwrap_or(0) > 0,
        "{r}"
    );
}

#[test]
fn the_cap_notice_appears_while_the_worker_still_runs() {
    let rig = Rig::new();
    // Flood stdout past the cap, then stay alive: a long-running worker.
    let script = rig.script(
        "chatty.js",
        "const line = 'x'.repeat(1023) + '\\n';\n\
         for (let i = 0; i < 64; i++) rt.print(line);\n\
         await new Promise(() => {});\n",
    );
    let mut child = Command::new(JSRT)
        .args([
            "supervise",
            script.to_str().unwrap_or_else(|| panic!("utf8")),
            "--timeout",
            "0",
            "--max-output-kb",
            "16",
        ])
        .current_dir(rig.dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn supervise: {e}"));
    let stderr = child
        .stderr
        .take()
        .unwrap_or_else(|| panic!("no stderr pipe"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut seen = false;
    while !seen {
        let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
            break;
        };
        match rx.recv_timeout(left) {
            Ok(line) => seen = line.contains("worker stdout reached the 16384-byte cap"),
            Err(_) => break,
        }
    }
    // Stop the supervised run either way, then judge.
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .output();
    let _ = child.wait();
    assert!(seen, "no cap notice while the worker was running");
}

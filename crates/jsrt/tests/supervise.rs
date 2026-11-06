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
        .unwrap_or_else(|| panic!("killed by signal"))
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

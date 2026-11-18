//! `jsrt supervise` — the worker-process boundary, proven end to end.
//!
//! The in-process watchdog can stop JavaScript; it cannot stop allocation
//! and it cannot outlive the isolate it watches. These tests spawn the real
//! binary supervising the real binary — budgets enforced by the kernel
//! (rlimits) or by footprint polling, never by a flag inside the runtime.

use std::fs;
use std::io::Read;
use std::os::fd::FromRawFd;
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

    fn supervise_env(&self, script: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
        let mut args = vec!["supervise"];
        args.push(script.to_str().unwrap_or_else(|| panic!("utf8")));
        args.extend_from_slice(extra);
        let mut cmd = Command::new(JSRT);
        cmd.args(&args).current_dir(self.dir.path());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap_or_else(|e| panic!("spawn jsrt: {e}"))
    }
}

fn code(out: &Output) -> i32 {
    out.status
        .code()
        .unwrap_or_else(|| panic!("killed by signal — status: {:?}", out.status))
}

/// Spawn `jsrt supervise <script>` with the supervisor's stdout and
/// stderr attached to a pty slave — so `is_terminal` sees a terminal —
/// and capture everything the pty master receives. Returns the exit code
/// and the captured bytes.
fn supervise_on_pty(script: &Path, extra: &[&str]) -> (i32, Vec<u8>) {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: openpty fills two `int` out-params with live fds on success;
    // null name/termios/winsize take the defaults.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty");
    // SAFETY: slave and master are live fds this test owns; File takes
    // ownership and closes them.
    let slave_out = unsafe { std::fs::File::from_raw_fd(slave) };
    let slave_err = slave_out
        .try_clone()
        .unwrap_or_else(|e| panic!("dup slave: {e}"));
    // SAFETY: master is a live fd this test owns; File takes ownership and
    // closes it.
    let mut master_file = unsafe { std::fs::File::from_raw_fd(master) };
    let mut child = Command::new(JSRT)
        .arg("supervise")
        .arg(script)
        .args(extra)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(slave_out))
        .stderr(std::process::Stdio::from(slave_err))
        .spawn()
        .unwrap_or_else(|e| panic!("spawn supervise: {e}"));
    // Drain the pty until EOF (or EIO when the last slave fd closes).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match master_file.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => captured.extend_from_slice(&buf[..n]),
            }
        }
        let _ = tx.send(captured);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                panic!("supervise on a pty did not finish in 15s");
            }
            Err(e) => panic!("wait: {e}"),
        }
    };
    let captured = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("pty reader did not finish: {e}"));
    (
        status
            .code()
            .unwrap_or_else(|| panic!("signalled: {status}")),
        captured,
    )
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
    // the in-process watchdog can only watch. Each chunk is written: a
    // fresh Uint8Array is zero pages nobody has touched, which Linux does
    // not count as resident, so an unwritten bomb stayed under the budget
    // on Linux until the worker's own timeout ended it.
    let script = rig.script(
        "bomb.js",
        "const c = [];\nfor (;;) c.push(new Uint8Array(8 * 1024 * 1024).fill(1));\n",
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
fn worker_stderr_lines_carry_the_run_id_and_stdout_stays_bare() {
    let rig = Rig::new();
    // The host's final error line is worker stderr; an operator reading a
    // shared log needs to know which run said it. stdout is a data pipe —
    // it stays byte-exact unless --prefix-stdout asks for the marker.
    let script = rig.script("die.js", "throw new Error('MARKED-LINE-7f3a');\n");
    let out = rig.supervise(&script, &[]);
    assert_eq!(code(&out), 5);
    let r = report(&out);
    let run_id = r["run_id"].as_str().unwrap_or_else(|| panic!("{r}"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let marked = stderr
        .lines()
        .find(|l| l.contains("MARKED-LINE-7f3a"))
        .unwrap_or_else(|| panic!("worker error line missing: {stderr}"));
    assert!(
        marked.starts_with(&format!("[run {run_id}]")),
        "worker stderr line is not prefixed with its run id: {marked}"
    );
    // The supervisor's own lines are not worker output — no prefix.
    let readiness = stderr
        .lines()
        .find(|l| l.contains("running, signals forwarded"))
        .unwrap_or_else(|| panic!("no readiness line: {stderr}"));
    assert!(
        !readiness.contains("[run "),
        "supervisor line got a worker prefix: {readiness}"
    );
    // stdout carries only the report — nothing prefixed.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("[run "),
        "stdout was prefixed without the flag: {stdout}"
    );
}

#[test]
fn prefix_stdout_marks_worker_lines_when_asked() {
    let rig = Rig::new();
    let script = rig.script("ok.js", "rt.print('worker-says-hi\\n');\n");
    let out = rig.supervise(&script, &["--prefix-stdout"]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = report(&out);
    let run_id = r["run_id"].as_str().unwrap_or_else(|| panic!("{r}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let marked = stdout
        .lines()
        .find(|l| l.contains("worker-says-hi"))
        .unwrap_or_else(|| panic!("worker line missing: {stdout}"));
    assert!(
        marked.starts_with(&format!("[run {run_id}]")),
        "worker stdout line is not prefixed: {marked}"
    );
}

#[test]
fn an_external_signal_is_reported_without_claiming_the_supervisor_sent_it() {
    // A signal that lands on the worker from outside (an operator, OOM
    // killer, another tool) is not a budget doing its job — the report
    // must name the signal and say the sender is unknown.
    let rig = Rig::new();
    let script = rig.script("stall.js", "await new Promise(() => {});\n");
    let mut child = Command::new(JSRT)
        .args([
            "supervise",
            script.to_str().unwrap_or_else(|| panic!("utf8")),
            "--timeout",
            "0",
        ])
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

    // Kill the worker itself — not the supervisor. The supervisor watches
    // a death it never ordered.
    Command::new("kill")
        .args(["-TERM", &worker_pid.to_string()])
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
    assert_eq!(r["signal"], 15, "{r}");
    assert_eq!(r["killed_by"], "signal 15 (sender unknown)", "{r}");
}

#[test]
fn worker_output_is_redacted_for_env_and_file_literals_and_counted() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    // Literal secrets come from the environment and a tight file — never
    // argv, which every local user can read through ps.
    let secrets = rig.dir.path().join("secrets.txt");
    fs::write(&secrets, "s3cr3t-file\n\n").unwrap_or_else(|e| panic!("write: {e}"));
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o600))
        .unwrap_or_else(|e| panic!("chmod: {e}"));
    let script = rig.script(
        "leak.js",
        "rt.print('token=hunter2 done\\n');\n\
         rt.print('file s3cr3t-file ok\\n');\n",
    );
    let path_s = secrets
        .to_str()
        .unwrap_or_else(|| panic!("utf8"))
        .to_string();
    let out = rig.supervise_env(
        &script,
        &["--redact-env", "JSRT_TEST_SECRET", "--redact-file", &path_s],
        &[("JSRT_TEST_SECRET", "hunter2")],
    );
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("hunter2"), "env secret leaked: {stdout}");
    assert!(
        !stdout.contains("s3cr3t-file"),
        "file secret leaked: {stdout}"
    );
    assert!(stdout.contains("token=[redacted] done"), "{stdout}");
    assert!(stdout.contains("file [redacted] ok"), "{stdout}");
    let r = report(&out);
    // hunter2 (7) + s3cr3t-file (11) masked.
    assert_eq!(r["output"]["stdout_redacted_bytes"], 18, "{r}");
    assert_eq!(r["output"]["redact_literals"], 2, "{r}");
}

#[test]
fn a_secret_split_across_pipe_chunks_is_still_redacted() {
    let rig = Rig::new();
    // The worker emits the secret in two writes separated by a sleep, so
    // it lands in two different reads of the forwarder. A naive
    // per-chunk match misses it; the hold-back boundary must not.
    let script = rig.script(
        "split.js",
        "rt.print('AAhun');\n\
         await rt.sleep(300);\n\
         rt.print('ter2BB\\n');\n",
    );
    let out = rig.supervise_env(
        &script,
        &["--redact-env", "JSRT_TEST_SECRET"],
        &[("JSRT_TEST_SECRET", "hunter2")],
    );
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("hunter2"), "secret leaked: {stdout}");
    assert!(stdout.contains("AA[redacted]BB"), "{stdout}");
    let r = report(&out);
    assert_eq!(r["output"]["stdout_redacted_bytes"], 7, "{r}");
}

#[test]
fn redaction_refuses_a_leaky_file_and_a_missing_env() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    let script = rig.script("ok.js", "rt.print('x\\n');\n");

    // A secrets file any local user can read cannot be trusted to hold
    // secrets — refuse it rather than mask halfheartedly.
    let secrets = rig.dir.path().join("leaky.txt");
    fs::write(&secrets, "hunter2\n").unwrap_or_else(|e| panic!("write: {e}"));
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o644))
        .unwrap_or_else(|e| panic!("chmod: {e}"));
    let path_s = secrets
        .to_str()
        .unwrap_or_else(|| panic!("utf8"))
        .to_string();
    let out = rig.supervise(&script, &["--redact-file", &path_s]);
    assert_eq!(
        code(&out),
        8,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("readable"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A named variable that does not exist protects nothing — refuse
    // instead of silently masking nothing.
    let out = rig.supervise(&script, &["--redact-env", "JSRT_TEST_DEFINITELY_UNSET"]);
    assert_eq!(
        code(&out),
        8,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("JSRT_TEST_DEFINITELY_UNSET"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn worker_escape_sequences_are_neutralised_on_a_tty_and_raw_on_a_pipe() {
    let rig = Rig::new();
    // An OSC window-title, a cursor clear, an OSC-8 link, and a sequence
    // split over two writes — the classic terminal-injection kit.
    let script = rig.script(
        "esc.js",
        "rt.print('\\x1b]0;owned\\x07move\\x1b[2Jlink\\x1b]8;;https://example.com\\x07here\\x1b]8;;\\x07\\n');\n\
         await rt.sleep(300);\n\
         rt.print('\\x1b');\n\
         await rt.sleep(300);\n\
         rt.print(']0;split\\x07\\n');\n",
    );
    let attack = b"\x1b]0;owned\x07";

    // Pipe: the consumer asked for the worker's bytes — the supervisor
    // passes them byte-exact, escapes and all.
    let out = rig.supervise(&script, &[]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout
            .windows(attack.len())
            .any(|w| w == attack.as_slice()),
        "pipe run rewrote worker bytes: {:?}",
        &out.stdout[..out.stdout.len().min(200)]
    );

    // PTY: every control byte is rendered visibly instead — the worker
    // cannot drive the operator's terminal, and the attempt stays visible.
    let (tty_code, captured) = supervise_on_pty(&script, &[]);
    assert_eq!(
        tty_code,
        0,
        "pty run failed: {}",
        String::from_utf8_lossy(&captured)
    );
    assert!(
        !captured.contains(&0x1b),
        "raw ESC reached the terminal: {}",
        String::from_utf8_lossy(&captured)
    );
    assert!(
        !captured.contains(&0x07),
        "raw BEL reached the terminal: {}",
        String::from_utf8_lossy(&captured)
    );
    let text = String::from_utf8_lossy(&captured);
    assert!(text.contains("^[]0;owned^G"), "{text}");
    assert!(text.contains("^[[2J"), "{text}");
    // Split across two pipe reads, the ESC is still rendered.
    assert!(text.contains("^[]0;split^G"), "{text}");

    // --output-raw on the same pty: a trusted worker keeps its bytes.
    let (raw_code, captured) = supervise_on_pty(&script, &["--output-raw"]);
    assert_eq!(
        raw_code,
        0,
        "raw pty run failed: {}",
        String::from_utf8_lossy(&captured)
    );
    assert!(
        captured
            .windows(attack.len())
            .any(|w| w == attack.as_slice()),
        "--output-raw did not pass bytes: {}",
        String::from_utf8_lossy(&captured)
    );
}

#[test]
fn worker_utf8_reaches_a_tty_byte_exact_and_c1_is_neutralised() {
    let rig = Rig::new();
    // Non-ASCII text must pass a terminal sink untouched — the bytes
    // 0x80..=0x9F are UTF-8 continuations in normal text. The real C1 is
    // U+009B, which a UTF-8 terminal receives as C2 9B.
    let script = rig.script(
        "utf8.js",
        "rt.print('caf\u{e9} \u{20ac} \u{1f600} \u{65e5}\u{672c}\u{8a9e} ');\n\
         rt.print('\\u009b');\n\
         rt.print('done\\n');\n",
    );
    let utf8_line = "caf\u{e9} \u{20ac} \u{1f600} \u{65e5}\u{672c}\u{8a9e} ".as_bytes();
    let csi = [0xc2u8, 0x9b];

    // Pipe: consumer wants the worker's bytes — UTF-8 and C1 alike pass.
    let out = rig.supervise(&script, &[]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.windows(utf8_line.len()).any(|w| w == utf8_line),
        "pipe rewrote UTF-8: {:?}",
        &out.stdout[..out.stdout.len().min(200)]
    );
    assert!(
        out.stdout.windows(2).any(|w| w == csi),
        "pipe lost the C1: {:?}",
        &out.stdout[..out.stdout.len().min(200)]
    );

    // PTY: the text is byte-exact and U+009B renders visibly as M-^[.
    let (tty_code, captured) = supervise_on_pty(&script, &[]);
    assert_eq!(
        tty_code,
        0,
        "pty run failed: {}",
        String::from_utf8_lossy(&captured)
    );
    assert!(
        captured.windows(utf8_line.len()).any(|w| w == utf8_line),
        "UTF-8 text corrupted on the terminal: {}",
        String::from_utf8_lossy(&captured)
    );
    assert!(
        !captured.windows(2).any(|w| w == csi),
        "raw C2 9B reached the terminal: {}",
        String::from_utf8_lossy(&captured)
    );
    let text = String::from_utf8_lossy(&captured);
    assert!(text.contains("M-^[done"), "{text}");
}

#[test]
fn overlapping_secrets_mask_their_union_not_the_second_secrets_tail() {
    let rig = Rig::new();
    // "12345678" contains "1234" and "345678": masking only the first
    // leaves "5678" — most of the second secret. The union must be masked.
    let secrets = rig.dir.path().join("secrets.txt");
    fs::write(&secrets, "1234\n345678\n").unwrap_or_else(|e| panic!("write: {e}"));
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o600))
        .unwrap_or_else(|e| panic!("chmod: {e}"));
    let path_s = secrets
        .to_str()
        .unwrap_or_else(|| panic!("utf8"))
        .to_string();
    let script = rig.script("overlap.js", "rt.print('pre 12345678 post\\n');\n");
    let out = rig.supervise(&script, &["--redact-file", &path_s]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("5678"),
        "tail of the second secret leaked: {stdout}"
    );
    assert!(stdout.contains("pre [redacted] post"), "{stdout}");
    let r = report(&out);
    assert_eq!(r["output"]["stdout_redacted_bytes"], 8, "{r}");
}

#[test]
fn redaction_refuses_tiny_or_unbounded_secret_sets() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    let script = rig.script("ok2.js", "rt.print('x\\n');\n");
    let mk = |name: &str, body: &str| -> String {
        let p = rig.dir.path().join(name);
        fs::write(&p, body).unwrap_or_else(|e| panic!("write: {e}"));
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|e| panic!("chmod: {e}"));
        p.to_str().unwrap_or_else(|| panic!("utf8")).to_string()
    };

    // A 3-byte literal would mask every unrelated occurrence.
    let tiny = mk("tiny.txt", "abc\n");
    let out = rig.supervise(&script, &["--redact-file", &tiny]);
    assert_eq!(
        code(&out),
        8,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("4 bytes"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The same footgun through the environment.
    let out = rig.supervise_env(
        &script,
        &["--redact-env", "JSRT_TEST_SECRET"],
        &[("JSRT_TEST_SECRET", "abc")],
    );
    assert_eq!(
        code(&out),
        8,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A secrets file is a scan pattern per output byte — bound the set.
    let many = mk(
        "many.txt",
        &(0..257)
            .map(|i| format!("secret-number-{i:04}-long-enough"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let out = rig.supervise(&script, &["--redact-file", &many]);
    assert_eq!(
        code(&out),
        8,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("256"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And bound the total bytes scanned for.
    let big = mk("big.txt", &format!("{}\n", "s".repeat(70000)));
    let out = rig.supervise(&script, &["--redact-file", &big]);
    assert_eq!(
        code(&out),
        8,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_steady_worker_under_the_rate_budget_flows_past_the_lifetime_cap() {
    let rig = Rig::new();
    // 16 x 1 KiB lines, one every ~100 ms (~10 KiB/s): past the 8 KiB
    // lifetime cap early, but under a 32 KiB/s rate every chunk is covered
    // by what the bucket accrued during the 100 ms gap. Without the rate
    // flag this is exit 2; with the old code it would be 8 KiB passed and
    // the rest dropped.
    let script = rig.script(
        "steady.js",
        "const line = 'x'.repeat(1023) + '\\n';\n\
         for (let i = 0; i < 16; i++) { rt.print(line); await rt.sleep(100); }\n",
    );
    let out = rig.supervise(
        &script,
        &["--max-output-kb", "8", "--max-output-rate-kbs", "32"],
    );
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = report(&out);
    assert_eq!(r["output"]["rate_bytes_per_sec"], 32 * 1024, "{r}");
    assert_eq!(r["output"]["stdout_bytes"], 16 * 1024, "{r}");
    assert_eq!(r["output"]["stdout_dropped_bytes"], 0, "{r}");
}

#[test]
fn a_flooding_worker_stays_bounded_when_a_rate_is_configured() {
    let rig = Rig::new();
    // 200 KiB at once against an 8 KiB cap + 64 KiB/s rate: the burst
    // spends the bucket, the flood finishes before the rate accrues
    // anything meaningful, and the drop is named in the new wording.
    let script = rig.script(
        "flood.js",
        "const line = 'x'.repeat(1023) + '\\n';\n\
         for (let i = 0; i < 200; i++) rt.print(line);\n",
    );
    let out = rig.supervise(
        &script,
        &["--max-output-kb", "8", "--max-output-rate-kbs", "64"],
    );
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = report(&out);
    let passed = r["output"]["stdout_bytes"].as_u64().unwrap_or(u64::MAX);
    // The whole run is well under a second, so at most cap + ~1s of rate
    // can have passed — never the flood itself.
    assert!(
        passed <= 8 * 1024 + 64 * 1024,
        "{passed} bytes passed — the rate let the flood through"
    );
    assert!(
        r["output"]["stdout_dropped_bytes"].as_u64().unwrap_or(0) > 0,
        "{r}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("exceeded its output budget"),
        "stderr tail: {}",
        &stderr[stderr.len().saturating_sub(400)..]
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

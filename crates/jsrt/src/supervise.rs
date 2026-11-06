//! `jsrt supervise` — the hard boundary an in-process watchdog can never be.
//!
//! `terminate_execution` can stop JavaScript; it cannot stop allocation, and
//! it cannot outlive the isolate it watches. The supervisor is the same
//! trick every production embedder uses: the runtime lives in a *child
//! process*, and the parent enforces what the child cannot enforce on
//! itself — CPU seconds via `setrlimit` (applied post-fork, before exec, so
//! the child cannot decline), resident memory by polling `proc_pid_rusage`,
//! and a wall-clock backstop over the whole thing.
//!
//! What the supervisor guarantees the in-process watchdog cannot:
//! - a memory bomb is an OS problem — the supervisor kills it and reports
//!   which budget tripped, instead of the host aborting itself;
//! - the supervisor survives whatever the worker does — SIGSEGV, SIGABRT,
//!   an unhandled path in deno_core — and still emits a classified report.
//!
//! Honest limits, written down: RSS polling is a bound, not a brick wall —
//! worst-case overage is (allocation rate × poll interval). `RLIMIT_AS` is
//! deliberately not used: V8 reserves a multi-GB pointer cage at startup,
//! so an address-space limit sized for RSS would prevent the engine from
//! booting. The memory budget is therefore *enforced on observed
//! footprint*, which is the only honest bound a V8 host can offer without
//! kernel accounting.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use jsrt_host::HostError;

/// How often the supervisor samples the child's resident footprint.
/// Worst-case overage is roughly `peak allocation rate × interval`.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Signals the supervisor considers "the budget did its job" — anything
/// else is an unexpected death the report must name, not launder.
const KILLED_BY_CPU: &str = "cpu-budget";
const KILLED_BY_RSS: &str = "rss-budget";
const KILLED_BY_WALL: &str = "wall-clock";

/// Set by the signal handler when the operator (or a process manager)
/// wants the supervised run down. The supervisor forwards it to the
/// worker — a supervisor that can't be stopped isn't supervision.
static OPERATOR_TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn on_operator_term(_sig: libc::c_int) {
    // Atomic store is async-signal-safe — the handler only raises a flag.
    OPERATOR_TERM.store(true, Ordering::SeqCst);
}

pub struct SupervisePlan {
    /// Arguments to pass to `jsrt run` verbatim (already serialized).
    pub worker_args: Vec<String>,
    /// Resident-footprint budget; over it, the worker dies.
    pub max_rss_bytes: Option<u64>,
    /// `RLIMIT_CPU` seconds; the kernel delivers SIGXCPU when exceeded.
    pub max_cpu_secs: Option<u64>,
    /// Optional wall-clock backstop for the whole supervised run.
    pub wall: Option<Duration>,
    /// Grace between SIGTERM and SIGKILL.
    pub kill_grace: Duration,
    /// Where the JSON run report goes — a file, or stdout.
    pub report_path: Option<PathBuf>,
    /// Byte cap for the report file before it rotates.
    pub report_max_bytes: u64,
}

/// Execute the supervision plan. Returns the process exit code jsrt
/// should use — the worker's class, or `terminated` (6) when a budget
/// did the killing.
pub fn supervise(plan: &SupervisePlan) -> Result<i32, HostError> {
    // A supervisor that can't be stopped isn't supervision: SIGTERM/SIGINT
    // to the supervisor forwards to the worker through the same
    // SIGTERM→SIGKILL path the budgets use, recorded as operator-sigterm.
    // The handler is armed *before* the worker exists — a TERM that lands
    // while the worker is still spawning sets the flag and forwards the
    // moment the poll loop starts. The only uncovered window left is the
    // process's own startup, which no design removes.
    OPERATOR_TERM.store(false, Ordering::SeqCst);
    unsafe {
        if libc::signal(
            libc::SIGTERM,
            on_operator_term as *const () as libc::sighandler_t,
        ) == libc::SIG_ERR
        {
            return Err(HostError::Host(format!(
                "supervise: signal(SIGTERM): {}",
                std::io::Error::last_os_error()
            )));
        }
        if libc::signal(
            libc::SIGINT,
            on_operator_term as *const () as libc::sighandler_t,
        ) == libc::SIG_ERR
        {
            return Err(HostError::Host(format!(
                "supervise: signal(SIGINT): {}",
                std::io::Error::last_os_error()
            )));
        }
    }

    let started = Instant::now();
    let run_id = format!(
        "{}x{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        std::process::id()
    );

    let exe = std::env::current_exe()
        .map_err(|e| HostError::Host(format!("supervise: own binary: {e}")))?;

    let mut cmd = Command::new(&exe);
    cmd.arg("run").args(&plan.worker_args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    // rlimits land post-fork pre-exec: the budget is set by the parent on
    // the child, so the child cannot decline it. Only CPU is rlimit-able —
    // RLIMIT_AS would stop V8's pointer-cage reservation before boot.
    if let Some(cpu) = plan.max_cpu_secs {
        unsafe {
            cmd.pre_exec(move || {
                let lim = libc::rlimit {
                    rlim_cur: cpu,
                    rlim_max: libc::RLIM_INFINITY,
                };
                if libc::setrlimit(libc::RLIMIT_CPU, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = cmd
        .spawn()
        .map_err(|e| HostError::Host(format!("supervise: spawn {}: {e}", exe.display())))?;
    let pid = child.id() as libc::pid_t;
    // The handle stays alive purely so the child stays ours until reaped —
    // dropping it would orphan nothing (Child::drop doesn't reap), but the
    // pid is only safe to use while we hold the handle.
    let _child_guard = child;

    // Operator-facing lifecycle line: the supervisor is armed and the
    // worker exists — the point at which SIGTERM/SIGINT forward cleanly.
    eprintln!("jsrt: supervise: worker {pid} running, signals forwarded");

    let mut peak_rss: u64 = 0;
    let mut killed_by: Option<&'static str> = None;
    // Two-stage kill, same shape as the in-process watchdog one level down:
    // SIGTERM, then SIGKILL after grace. `reap()` in this loop is the only
    // reaper — a second one anywhere would hit ECHILD on an already-reaped
    // zombie.
    let mut sigterm_at: Option<Instant> = None;
    let mut sigkill_sent = false;

    let (status, usage) = loop {
        match reap(pid) {
            Reap::Exited(s, ru) => break (s, ru),
            Reap::Running => {}
            Reap::Error(e) => {
                return Err(HostError::Host(format!("supervise: wait4: {e}")));
            }
        }

        if let Some(rss) = resident_bytes(pid) {
            peak_rss = peak_rss.max(rss);
            if let Some(budget) = plan.max_rss_bytes {
                if rss > budget && sigterm_at.is_none() {
                    killed_by = Some(KILLED_BY_RSS);
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    sigterm_at = Some(Instant::now());
                }
            }
        }

        if let Some(wall) = plan.wall {
            if started.elapsed() > wall && sigterm_at.is_none() {
                killed_by = Some(KILLED_BY_WALL);
                unsafe { libc::kill(pid, libc::SIGTERM) };
                sigterm_at = Some(Instant::now());
            }
        }

        // The operator asked the supervisor to stop — forward through the
        // same path the budgets take.
        if OPERATOR_TERM.load(Ordering::SeqCst) && sigterm_at.is_none() {
            killed_by = Some("operator-sigterm");
            unsafe { libc::kill(pid, libc::SIGTERM) };
            sigterm_at = Some(Instant::now());
        }

        // Grace expired and the worker still breathes — escalate.
        if !sigkill_sent {
            if let Some(at) = sigterm_at {
                if at.elapsed() > plan.kill_grace {
                    sigkill_sent = true;
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    };

    // Prefer the kernel's peak-RSS over our sampled peak — the report
    // should carry the highest value observed by anyone, not just our poll
    // cadence. `ru_maxrss` is bytes on Darwin, KiB on Linux.
    #[cfg(target_os = "macos")]
    let kernel_peak = usage.ru_maxrss as u64;
    #[cfg(target_os = "linux")]
    let kernel_peak = (usage.ru_maxrss as u64).saturating_mul(1024);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let kernel_peak = 0u64;
    let peak_rss = peak_rss.max(kernel_peak);
    let cpu_ms = (usage.ru_utime.tv_sec as u64 * 1000 + usage.ru_utime.tv_usec as u64 / 1000)
        + (usage.ru_stime.tv_sec as u64 * 1000 + usage.ru_stime.tv_usec as u64 / 1000);
    let wall_ms = started.elapsed().as_millis() as u64;

    let (outcome, exit_code, effective_killed_by) = classify_death(status, killed_by);

    let report = json!({
        "run_id": run_id,
        "outcome": outcome.label,
        "exit_code": exit_code,
        "worker_pid": pid,
        "wall_ms": wall_ms,
        "cpu_ms": cpu_ms,
        "peak_rss_bytes": peak_rss,
        "limits": {
            "max_rss_bytes": plan.max_rss_bytes,
            "max_cpu_secs": plan.max_cpu_secs,
            "wall_ms": plan.wall.map(|d| d.as_millis() as u64),
        },
        "killed_by": effective_killed_by,
        "signal": outcome.signal,
        "poll_interval_ms": POLL_INTERVAL.as_millis(),
    });

    emit(&report, plan.report_path.as_deref(), plan.report_max_bytes)?;
    Ok(exit_code)
}

struct Outcome {
    label: &'static str,
    signal: Option<i32>,
}

/// Map the child's wait-status onto the run taxonomy. A signal we sent is
/// a budget doing its job; a signal nobody asked for is a host failure —
/// the report names it either way. Returns the label, the exit code, and
/// the `killed_by` the report should carry (SIGXCPU explains itself).
fn classify_death(
    status: libc::c_int,
    killed_by: Option<&'static str>,
) -> (Outcome, i32, Option<&'static str>) {
    if libc::WIFEXITED(status) {
        let code = libc::WEXITSTATUS(status);
        let label = match code {
            0 => "clean",
            2 => "usage",
            3 => "load",
            4 => "syntax",
            5 => "exception",
            6 => "terminated",
            7 => "capability",
            8 => "host",
            _ => "exited",
        };
        return (
            Outcome {
                label,
                signal: None,
            },
            code,
            killed_by,
        );
    }
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        // A SIGXCPU is the CPU rlimit, a SIGTERM/SIGKILL after kill_sent is
        // our own hand, a SIGABRT is most likely V8's OOM handler firing —
        // all budget-class deaths. Anything else is unexplained.
        let by = killed_by.or(if sig == libc::SIGXCPU {
            Some(KILLED_BY_CPU)
        } else if sig == libc::SIGABRT {
            Some("abort")
        } else {
            None
        });
        let expected = killed_by.is_some()
            || sig == libc::SIGXCPU
            || sig == libc::SIGABRT
            || sig == libc::SIGTERM
            || sig == libc::SIGKILL;
        let label = if expected { "terminated" } else { "host" };
        let code = if expected { 6 } else { 8 };
        return (
            Outcome {
                label,
                signal: Some(sig),
            },
            code,
            by,
        );
    }
    (
        Outcome {
            label: "host",
            signal: None,
        },
        8,
        killed_by,
    )
}

enum Reap {
    Exited(libc::c_int, libc::rusage),
    Running,
    Error(std::io::Error),
}

/// `wait4(WNOHANG)` — the only wait that hands back both the exit status
/// and the child's peak resource usage.
fn reap(pid: libc::pid_t) -> Reap {
    let mut status: libc::c_int = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut usage) };
    if ret == 0 {
        Reap::Running
    } else if ret < 0 {
        Reap::Error(std::io::Error::last_os_error())
    } else {
        Reap::Exited(status, usage)
    }
}

/// Current resident footprint. `proc_pid_rusage` is the honest source on
/// Darwin — the same number Activity Monitor shows, not a VM accounting.
#[cfg(target_os = "macos")]
fn resident_bytes(pid: libc::pid_t) -> Option<u64> {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            &mut info as *mut libc::rusage_info_v4 as *mut libc::rusage_info_t,
        )
    };
    if ret == 0 {
        Some(info.ri_phys_footprint)
    } else {
        None
    }
}

/// `/proc/<pid>/status` VmRSS on Linux — same role, different door.
#[cfg(target_os = "linux")]
fn resident_bytes(pid: libc::pid_t) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line
        .trim_start_matches("VmRSS:")
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb * 1024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resident_bytes(_pid: libc::pid_t) -> Option<u64> {
    None
}

fn emit(
    report: &serde_json::Value,
    path: Option<&std::path::Path>,
    max_bytes: u64,
) -> Result<(), HostError> {
    let line = serde_json::to_string(report)
        .map_err(|e| HostError::Host(format!("report serialize: {e}")))?;
    match path {
        Some(p) => append_bounded(p, &line, max_bytes),
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            writeln!(lock, "{line}").map_err(|e| HostError::Host(format!("report write: {e}")))
        }
    }
}

/// Append one JSONL record under a byte cap. When the next record would
/// overflow, the file rotates to `<name>.1` and the fresh file opens with
/// a meta-record saying how many records the rotation displaced — a
/// flooded log limits itself and *says so*, which is the whole point of
/// bounded diagnostics. One generation is kept; a single record larger
/// than the cap is written whole to a fresh file — boundedness never eats
/// a run's only evidence.
fn append_bounded(path: &std::path::Path, line: &str, max_bytes: u64) -> Result<(), HostError> {
    let current_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if current_len > 0 && current_len + line.len() as u64 + 1 > max_bytes {
        rotate(path).map_err(|e| HostError::Host(format!("report rotate: {e}")))?;
    }
    append_line(path, line)
}

/// Rename `path` to `<name>.1` and open a fresh file whose first record is
/// the rotation itself — carrying the count of records displaced.
fn rotate(path: &std::path::Path) -> std::io::Result<u64> {
    let dropped = count_records(path);
    let rotated = path.with_file_name(format!(
        "{}.1",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    std::fs::rename(path, &rotated)?;
    let meta = json!({
        "type": "rotation",
        "dropped_records": dropped,
        "rotated_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    let line = serde_json::to_string(&meta).map_err(|e| std::io::Error::other(e.to_string()))?;
    append_line(path, &line).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(dropped)
}

fn count_records(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count() as u64)
        .unwrap_or(0)
}

fn append_line(path: &std::path::Path, line: &str) -> Result<(), HostError> {
    use std::fs::OpenOptions;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| HostError::Host(format!("report file {}: {e}", path.display())))?;
    writeln!(f, "{line}").map_err(|e| HostError::Host(format!("report file: {e}")))
}

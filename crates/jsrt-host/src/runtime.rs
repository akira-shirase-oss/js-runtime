//! Runtime construction and the honest run lifecycle.
//!
//! The timeout story is the part most tutorials get wrong, so it's spelled
//! out: `tokio::time::timeout` around the event loop cannot preempt a
//! CPU-bound script — the executor thread is blocked inside the poll. The
//! watchdog instead calls `IsolateHandle::terminate_execution()` from a
//! separate thread, which interrupts running JS *and* stalls the event loop.
//! A second `tokio::timeout` backstops a fully-idle loop. Memory bombs need a
//! process boundary — that's the supervisor milestone, documented in ROADMAP,
//! not claimed here.

use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use deno_core::error::CoreErrorKind;
use deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions};
use deno_error::JsErrorClass;

use crate::capability::Capabilities;
use crate::error::HostError;
use crate::ops::jsrt_core;

/// How one run is allowed to behave.
#[derive(Debug, Clone)]
pub struct HostOptions {
    /// The capability manifest applied to every op. `deny_all()` is the
    /// correct default.
    pub capabilities: Capabilities,
    /// Wall-clock deadline for the whole run — module load, evaluation and
    /// the event loop. `None` means "until it finishes"; the CLI always sets
    /// one unless `--timeout 0` is passed explicitly.
    pub timeout: Option<Duration>,
    /// If the watchdog fires and the run still hasn't unwound after this
    /// grace, escalate to `process::exit` with the terminated code. This is
    /// the in-process stand-in for the supervisor's hard kill: it exists
    /// because a *parked* event loop (a promise that never resolves) has no
    /// executing JS for `terminate_execution` to interrupt. CLI-only —
    /// library embedders keep graceful semantics and handle stalls in their
    /// own supervisor.
    pub terminate_grace: Option<Duration>,
    /// Per-fetch deadline. `None` uses the built-in 10s bound — a script may
    /// never hold a network op open longer than this.
    pub fetch_deadline: Option<Duration>,
}

/// What a run left behind when it ended, counted from the activity tracker
/// the moment before the isolate drops. These die with the runtime — the
/// point of reporting them is that a leak is *seen*, not silently dropped.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PendingCounts {
    pub timers: usize,
    pub intervals: usize,
    pub ops: usize,
    pub resources: usize,
}

/// The run's outcome plus what was still open at teardown.
#[derive(Debug)]
pub struct RunReport {
    pub result: Result<(), HostError>,
    pub pending: PendingCounts,
}

/// Count pending JS-visible activity just before the isolate drops.
fn pending_counts(runtime: &JsRuntime) -> PendingCounts {
    let snapshot = runtime
        .runtime_activity_stats_factory()
        .capture(&deno_core::stats::RuntimeActivityStatsFilter::all())
        .dump();
    let mut counts = PendingCounts::default();
    for activity in snapshot.active {
        match activity.activity() {
            deno_core::stats::RuntimeActivityType::Timer => counts.timers += 1,
            deno_core::stats::RuntimeActivityType::Interval => counts.intervals += 1,
            deno_core::stats::RuntimeActivityType::AsyncOp => counts.ops += 1,
            _ => counts.resources += 1,
        }
    }
    counts
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            capabilities: Capabilities::deny_all(),
            timeout: Some(Duration::from_secs(10)),
            terminate_grace: None,
            fetch_deadline: None,
        }
    }
}

/// Build a runtime wired with our ops and the given capabilities.
///
/// `entry_dir` is the program's own directory — an implicit read path for
/// the *module loader only*. The operator chose this program, so its local
/// module graph is program-space; ops still see only the manifest's grants.
fn build_runtime(options: &HostOptions, entry_dir: Option<&Path>) -> JsRuntime {
    let mut grants = options.capabilities.clone().into_grants();
    if let Some(dir) = entry_dir.and_then(|d| d.canonicalize().ok()) {
        grants.read_paths.push(dir);
    }
    let loader_capabilities = Capabilities::from_grants(grants);
    let runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(crate::module_loader::gated_loader(&loader_capabilities)),
        extensions: vec![jsrt_core::init()],
        ..Default::default()
    });
    let state = runtime.op_state();
    {
        let mut state = state.borrow_mut();
        state.put(Rc::new(options.capabilities.clone()));
        state.put(Rc::new(reqwest::Client::new()));
        let budgets = crate::ops::OpBudgets {
            fetch_deadline: options.fetch_deadline.unwrap_or(crate::ops::FETCH_DEADLINE),
            ..Default::default()
        };
        state.put(Rc::new(budgets));
        // The KV grant becomes a state cell only when declared — the op
        // finds no cell and refuses, same door as every other capability.
        if let Ok(grant) = options.capabilities.may_kv() {
            state.put(Rc::new(std::cell::RefCell::new(crate::ops::KvState::new(
                grant,
            ))));
        }
    }
    runtime
}

/// Run an ES module file to completion: evaluate it, drive the event loop
/// until pending work settles, and classify every way it can end.
pub async fn run_file(entry: &Path, options: &HostOptions) -> Result<(), HostError> {
    run_file_report(entry, options).await.result
}

/// `run_file` with the teardown report — what was still open when the run
/// ended, counted before the isolate dropped it.
pub async fn run_file_report(entry: &Path, options: &HostOptions) -> RunReport {
    if !entry.is_file() {
        return RunReport {
            result: Err(HostError::Load(format!(
                "no such file: {}",
                entry.display()
            ))),
            pending: PendingCounts::default(),
        };
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let specifier = deno_core::resolve_path(&entry.to_string_lossy(), &cwd)
        .map_err(|e| HostError::Load(format!("cannot resolve {}: {e}", entry.display())));
    let specifier = match specifier {
        Ok(s) => s,
        Err(e) => {
            return RunReport {
                result: Err(e),
                pending: PendingCounts::default(),
            };
        }
    };

    let mut runtime = build_runtime(options, entry.parent());
    let done = Arc::new(AtomicBool::new(false));
    let terminated = spawn_watchdog(
        &mut runtime,
        options.timeout,
        options.terminate_grace,
        Arc::clone(&done),
    );

    let result = async {
        let module_id = runtime
            .load_main_es_module(&specifier)
            .await
            .map_err(|e| classify(&e, &terminated))?;
        let evaluation = runtime.mod_evaluate(module_id);
        drive(&mut runtime, evaluation, options.timeout, terminated).await
    }
    .await;
    // Whatever the outcome, the run is over — the watchdog must not fire into
    // a dropped isolate or escalate against a finished process.
    done.store(true, Ordering::SeqCst);
    let pending = pending_counts(&runtime);
    RunReport { result, pending }
}

/// Run an in-memory module — the test path. Same lifecycle as `run_file`.
pub async fn run_source(name: &str, source: &str, options: &HostOptions) -> Result<(), HostError> {
    run_source_report(name, source, options).await.result
}

/// `run_source` with the teardown report.
pub async fn run_source_report(name: &str, source: &str, options: &HostOptions) -> RunReport {
    let specifier = ModuleSpecifier::parse(&format!("file:///{name}"))
        .map_err(|e| HostError::Load(format!("bad specifier {name}: {e}")));
    let specifier = match specifier {
        Ok(s) => s,
        Err(e) => {
            return RunReport {
                result: Err(e),
                pending: PendingCounts::default(),
            };
        }
    };
    // An embedded run is "as if" run from cwd — same entry-dir convention.
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let mut runtime = build_runtime(options, Some(cwd.as_path()));
    let done = Arc::new(AtomicBool::new(false));
    let terminated = spawn_watchdog(
        &mut runtime,
        options.timeout,
        options.terminate_grace,
        Arc::clone(&done),
    );
    let result = async {
        let module_id = runtime
            .load_main_es_module_from_code(&specifier, source.to_string())
            .await
            .map_err(|e| classify(&e, &terminated))?;
        let evaluation = runtime.mod_evaluate(module_id);
        drive(&mut runtime, evaluation, options.timeout, terminated).await
    }
    .await;
    done.store(true, Ordering::SeqCst);
    let pending = pending_counts(&runtime);
    RunReport { result, pending }
}

/// Spawn the termination watchdog and return the flag it sets when it fires.
/// `terminate_execution` is the only mechanism that stops JS *while it runs
/// on this isolate* — a `select!`-style cancel can never see a `while(1){}`.
/// Stage two, when `terminate_grace` is set: a parked event loop has no JS to
/// interrupt, so if the run still hasn't finished after the grace the watchdog
/// exits the process itself — same ending the supervisor will enforce with a
/// signal, minus the process boundary.
fn spawn_watchdog(
    runtime: &mut JsRuntime,
    timeout: Option<Duration>,
    terminate_grace: Option<Duration>,
    done: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
    let fired = Arc::new(AtomicBool::new(false));
    if let Some(deadline) = timeout {
        let handle = runtime.v8_isolate().thread_safe_handle();
        let fired_thread = Arc::clone(&fired);
        std::thread::spawn(move || {
            std::thread::sleep(deadline);
            if done.load(Ordering::SeqCst) {
                return;
            }
            fired_thread.store(true, Ordering::SeqCst);
            handle.terminate_execution();
            if let Some(grace) = terminate_grace {
                std::thread::sleep(grace);
                if !done.load(Ordering::SeqCst) {
                    eprintln!(
                        "jsrt: terminated: run did not unwind {}ms after termination",
                        grace.as_millis()
                    );
                    std::process::exit(HostError::Terminated(String::new()).exit_code());
                }
            }
        });
    }
    fired
}

/// Drive the event loop and the evaluation future to settlement, with a
/// tokio-side backstop for a fully idle loop the terminator can't reach.
async fn drive(
    runtime: &mut JsRuntime,
    evaluation: impl std::future::Future<Output = Result<(), deno_core::error::CoreError>>,
    timeout: Option<Duration>,
    terminated: Arc<AtomicBool>,
) -> Result<(), HostError> {
    let deadline = timeout.map(|t| t + Duration::from_secs(2));
    let options = PollEventLoopOptions::default();
    let loop_result = match deadline {
        Some(d) => match tokio::time::timeout(d, runtime.run_event_loop(options)).await {
            Ok(r) => r.map_err(|e| classify(&e, &terminated)),
            Err(_) => Err(HostError::Terminated(
                "event loop did not settle within deadline".into(),
            )),
        },
        None => runtime
            .run_event_loop(options)
            .await
            .map_err(|e| classify(&e, &terminated)),
    };
    let eval_result = evaluation.await.map_err(|e| classify(&e, &terminated));
    match (loop_result, eval_result) {
        // Termination beats whatever the script was doing when it died.
        (Err(HostError::Terminated(m)), _) | (_, Err(HostError::Terminated(m))) => {
            Err(HostError::Terminated(m))
        }
        (Err(e), _) | (_, Err(e)) => Err(e),
        (Ok(()), Ok(_)) => Ok(()),
    }
}

/// The classifier every host error flows through. Order matters: an isolate
/// that was terminated reports its own death first.
fn classify(err: &deno_core::error::CoreError, terminated: &AtomicBool) -> HostError {
    if terminated.load(Ordering::SeqCst) {
        return HostError::Terminated("execution terminated by timeout watchdog".into());
    }
    match err.as_kind() {
        CoreErrorKind::ExecutionTerminated => HostError::Terminated("execution terminated".into()),
        CoreErrorKind::Io(io_err) => HostError::Load(io_err.to_string()),
        CoreErrorKind::Js(js) => {
            let name = js.name.as_deref().unwrap_or("Error");
            // `exception_message` already carries V8's "Uncaught"/"Uncaught
            // (in promise)" prefix — strip it so ours isn't doubled.
            let detail = js
                .exception_message
                .trim_start_matches("Uncaught (in promise) ")
                .trim_start_matches("Uncaught ")
                .to_string();
            // Dynamic-import refusals reach here flattened: V8 rejects the
            // import promise with a TypeError carrying our message, losing
            // the CapabilityError class. The loader is the only emitter of
            // the prefix, so an uncaught one still exits 7. `detail` is
            // "{name}: {message}" — skip the class name to reach it.
            if detail
                .strip_prefix(&format!("{name}: "))
                .is_some_and(|m| m.starts_with(crate::module_loader::IMPORT_DENIED_PREFIX))
            {
                return HostError::Capability(format!("CapabilityError: {detail}"));
            }
            match name {
                "SyntaxError" => HostError::Syntax(detail),
                "CapabilityError" => HostError::Capability(format!("{name}: {detail}")),
                "HostError" => HostError::Host(format!("{name}: {detail}")),
                // `detail` already carries the error's own "Error: msg" form.
                _ => HostError::Exception(detail),
            }
        }
        // Module-machinery failures (missing deps, bad specifiers) arrive as
        // structured Rust errors, not JS exceptions — their Display names the
        // failing specifier, which is what the operator needs. Registered op
        // classes still take precedence if one ever surfaces this way.
        CoreErrorKind::JsBox(jsbox) => match jsbox.get_class().as_ref() {
            "CapabilityError" => HostError::Capability(jsbox.to_string()),
            "HostError" => HostError::Host(jsbox.to_string()),
            _ => HostError::Load(jsbox.to_string()),
        },
        _ => HostError::Exception(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use deno_core::ModuleSpecifier;

    /// A syntax error inside an imported file must classify as Syntax, not
    /// Load or Exception — run_source can't import files, so this needs the
    /// real loader path and lives here instead of in lifecycle.rs.
    #[tokio::test]
    async fn syntax_error_in_dependency_is_syntax() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        for (name, src) in [
            ("dep.js", "const = 3;\n"),
            ("main.js", "import './dep.js';\n"),
        ] {
            std::fs::write(dir.path().join(name), src)
                .unwrap_or_else(|e| panic!("write {name}: {e}"));
        }
        let options = super::HostOptions::default();
        let specifier = ModuleSpecifier::from_file_path(dir.path().join("main.js"))
            .unwrap_or_else(|e| panic!("specifier: {e:?}"));
        let mut runtime = super::build_runtime(&options, Some(dir.path()));
        let err = match runtime.load_main_es_module(&specifier).await {
            Err(e) => e,
            Ok(_) => panic!("must fail"),
        };
        let classified = super::classify(&err, &std::sync::atomic::AtomicBool::new(false));
        assert!(
            matches!(classified, super::HostError::Syntax(_)),
            "expected Syntax, got {classified:?}"
        );
    }

    // ---- M2.3: what a script leaves open is counted, and dies with the run.

    /// A timer a script abandons is *seen* at teardown — an unref'd pending
    /// timer shows up in the report and then dies with the isolate.
    #[tokio::test]
    async fn an_abandoned_unrefed_timer_is_counted_at_teardown() {
        let report = super::run_source_report(
            "leak.js",
            // Two unref'd pending timers (the loop may leave, they stay
            // tracked) plus a cleared one that must NOT be counted.
            "const a = rt.setTimeout(() => {}, 60000);\n\
             const b = rt.setTimeout(() => {}, 60000);\n\
             rt.unrefTimer(a);\n\
             rt.unrefTimer(b);\n\
             rt.clearTimeout(rt.setTimeout(() => {}, 1));",
            &super::HostOptions::default(),
        )
        .await;
        assert!(report.result.is_ok(), "run: {:?}", report.result);
        assert_eq!(
            report.pending.timers, 2,
            "both unref'd timers still tracked, cleared one gone: {:?}",
            report.pending
        );
    }

    /// A pending *interval* holds the loop; the watchdog ends the run, and the
    /// report still names the interval that was in flight when it died.
    #[tokio::test]
    async fn a_pending_interval_is_reported_on_termination() {
        let options = super::HostOptions {
            timeout: Some(std::time::Duration::from_millis(200)),
            ..Default::default()
        };
        let report =
            super::run_source_report("leak.js", "rt.setInterval(() => {}, 5000);", &options).await;
        assert!(
            matches!(report.result, Err(super::HostError::Terminated(_))),
            "expected Terminated, got {:?}",
            report.result
        );
        assert!(
            report.pending.intervals >= 1,
            "report: {:?}",
            report.pending
        );
    }

    /// Nothing outlives the runtime: a second run in the same process and
    /// tokio runtime completes cleanly — zombie work would show up here as
    /// interference or a hang.
    #[tokio::test]
    async fn a_second_run_after_teardown_is_unaffected() {
        let options = super::HostOptions::default();
        let first = super::run_source_report(
            "one.js",
            "const id = rt.setTimeout(() => {}, 60000); rt.unrefTimer(id);",
            &options,
        )
        .await;
        assert!(first.result.is_ok());
        let second = super::run_source("two.js", "rt.print('still fine');", &options).await;
        assert!(second.is_ok(), "second run: {second:?}");
    }
}

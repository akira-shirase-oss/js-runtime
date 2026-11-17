//! Runtime construction and the honest run lifecycle.
//!
//! The timeout story is the part most tutorials get wrong, so it's spelled
//! out: `tokio::time::timeout` around the event loop cannot preempt a
//! CPU-bound script — the executor thread is blocked inside the poll. The
//! watchdog instead calls `IsolateHandle::terminate_execution()` from a
//! separate thread, which interrupts running JS *and* stalls the event loop.
//! A second `tokio::timeout` backstops a fully-idle loop.
//!
//! The V8 heap gets the same treatment: a near-heap-limit callback
//! terminates the isolate when the heap reaches its ceiling (`max_heap`, or
//! V8's default), so a runaway allocation ends the run as `Terminated`
//! instead of a V8 out-of-memory abort (a signal, no exit code). The
//! termination lands only when the running call returns, so the first time
//! the ceiling is reached V8 gets [`HEAP_TERMINATION_HEADROOM`] (1 GiB) more
//! for that call to finish; a call that allocates more than that past the
//! ceiling still aborts V8. That bounds the JavaScript heap only;
//! `ArrayBuffer` backing stores and host buffers are outside it, so memory
//! as a whole is still a process-boundary problem — the supervisor's RSS
//! budget, documented in ROADMAP.

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
    /// Ceiling for the V8 heap in bytes. `None` keeps V8's own default;
    /// a value below [`MIN_MAX_HEAP`] is raised to it, since V8 cannot start
    /// a script in less. Either way, reaching it ends the run as
    /// `Terminated` (exit 6) instead of letting V8 abort the process, as
    /// long as the call running at that moment allocates no more than
    /// [`HEAP_TERMINATION_HEADROOM`] past it before returning. It bounds the
    /// JavaScript heap only: `ArrayBuffer` backing stores and host-side
    /// buffers live outside it, which is why `jsrt supervise --max-rss-mb`
    /// stays the memory boundary.
    pub max_heap: Option<usize>,
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

/// Why the host stopped the isolate, if it did — set by the watchdog
/// thread or the heap callback, read by [`classify`].
#[derive(Debug, Default)]
pub(crate) struct Stopped {
    by_timeout: AtomicBool,
    by_heap: AtomicBool,
}

/// The smallest V8 heap ceiling a run gets. Below about 8 MiB V8 aborts
/// while it starts even a one-line script (measured: 6 MiB aborted, 8 MiB
/// ran); 16 MiB leaves room for the termination to unwind.
pub const MIN_MAX_HEAP: usize = 16 * 1024 * 1024;
/// Heap granted on top of the limit the first time V8 reaches it. The
/// isolate is told to terminate then, but termination lands only when the
/// running call returns: one large array, or a builtin such as `fill`
/// that allocates as it goes, needs room to finish first (a 50-million
/// slot `fill` took about 700 MB). 1 GiB also covers V8's largest single
/// object. The overshoot is transient: the run is already ending.
pub const HEAP_TERMINATION_HEADROOM: usize = 1024 * 1024 * 1024;
/// Heap granted past the limit, each later time V8 reaches it, so the
/// unwinding has room to finish.
const HEAP_UNWIND_HEADROOM: usize = 32 * 1024 * 1024;
/// How many times that headroom is granted. Past it V8 is left to abort:
/// a run that cannot unwind after this much has nothing left to save.
const HEAP_UNWIND_GRANTS: u32 = 4;

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
            max_heap: None,
        }
    }
}

/// Build a runtime wired with our ops and the given capabilities.
///
/// `entry_dir` is the program's own directory — an implicit source of
/// *JavaScript modules only* for the loader. The operator chose this
/// program, so its local module graph is program-space; text/bytes/JSON
/// imports and every op still see only the manifest's grants.
fn build_runtime(options: &HostOptions, entry_dir: Option<&Path>) -> (JsRuntime, Arc<Stopped>) {
    let mut runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(crate::module_loader::gated_loader(
            &options.capabilities,
            entry_dir,
        )),
        extensions: vec![jsrt_core::init(), crate::serve::jsrt_serve::init()],
        create_params: options.max_heap.map(|max| {
            deno_core::v8::CreateParams::default().heap_limits(0, max.max(MIN_MAX_HEAP))
        }),
        ..Default::default()
    });
    let stopped = Arc::new(Stopped::default());
    // At the heap ceiling: terminate the isolate, and give the call that
    // is running room to return so the termination can land. V8 calls this
    // again if the unwinding allocates up to the new limit; after
    // HEAP_UNWIND_GRANTS more the limit stays and V8 aborts.
    let handle = runtime.v8_isolate().thread_safe_handle();
    let heap_stop = Arc::clone(&stopped);
    let mut grants = 0u32;
    runtime.add_near_heap_limit_callback(move |current, _initial| {
        if !heap_stop.by_heap.swap(true, Ordering::SeqCst) {
            handle.terminate_execution();
            return current.saturating_add(HEAP_TERMINATION_HEADROOM);
        }
        if grants >= HEAP_UNWIND_GRANTS {
            return current;
        }
        grants += 1;
        current.saturating_add(HEAP_UNWIND_HEADROOM)
    });
    let state = runtime.op_state();
    {
        let mut state = state.borrow_mut();
        state.put(Rc::new(options.capabilities.clone()));
        // The fetch client re-checks the manifest on every redirect hop and
        // never routes through an environment proxy — see net.rs.
        match crate::net::client(&options.capabilities) {
            Ok(client) => state.put(Rc::new(client)),
            Err(e) => state.put(Rc::new(crate::net::ClientUnavailable(e.to_string()))),
        }
        let budgets = crate::ops::OpBudgets {
            fetch_deadline: options.fetch_deadline.unwrap_or(crate::ops::FETCH_DEADLINE),
            ..Default::default()
        };
        state.put(Rc::new(budgets));
        state.put(Rc::new(crate::ops::FetchSlots::default()));
        // The KV grant becomes a state cell only when declared — the op
        // finds no cell and refuses, same door as every other capability.
        if let Ok(grant) = options.capabilities.may_kv() {
            state.put(Rc::new(std::cell::RefCell::new(crate::ops::KvState::new(
                grant,
            ))));
        }
        if let Ok(grant) = options.capabilities.may_media() {
            state.put(Rc::new(grant));
        }
        if let Ok(grant) = options.capabilities.may_serve() {
            state.put(Rc::new(grant));
        }
    }
    (runtime, stopped)
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

    // `main.js` has the parent "", which is the working directory; left
    // as it is, it would not canonicalize and the program would lose its
    // own directory as module space.
    let entry_dir = match entry.parent() {
        Some(dir) if dir.as_os_str().is_empty() => Some(Path::new(".")),
        dir => dir,
    };
    let (mut runtime, stopped) = build_runtime(options, entry_dir);
    let done = Arc::new(AtomicBool::new(false));
    spawn_watchdog(
        &mut runtime,
        options.timeout,
        options.terminate_grace,
        Arc::clone(&done),
        Arc::clone(&stopped),
    );

    let result = async {
        let module_id = runtime
            .load_main_es_module(&specifier)
            .await
            .map_err(|e| classify(&e, &stopped))?;
        let evaluation = runtime.mod_evaluate(module_id);
        drive(&mut runtime, evaluation, options.timeout, &stopped).await
    }
    .await;
    // Whatever the outcome, the run is over — the watchdog must not fire into
    // a dropped isolate or escalate against a finished process.
    done.store(true, Ordering::SeqCst);
    // The script may have died before its listener closed; the host still
    // reports the serve log lines it held back.
    crate::serve::flush_serve_log(&mut runtime.op_state().borrow_mut());
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
    let (mut runtime, stopped) = build_runtime(options, Some(cwd.as_path()));
    let done = Arc::new(AtomicBool::new(false));
    spawn_watchdog(
        &mut runtime,
        options.timeout,
        options.terminate_grace,
        Arc::clone(&done),
        Arc::clone(&stopped),
    );
    let result = async {
        let module_id = runtime
            .load_main_es_module_from_code(&specifier, source.to_string())
            .await
            .map_err(|e| classify(&e, &stopped))?;
        let evaluation = runtime.mod_evaluate(module_id);
        drive(&mut runtime, evaluation, options.timeout, &stopped).await
    }
    .await;
    done.store(true, Ordering::SeqCst);
    // The script may have died before its listener closed; the host still
    // reports the serve log lines it held back.
    crate::serve::flush_serve_log(&mut runtime.op_state().borrow_mut());
    let pending = pending_counts(&runtime);
    RunReport { result, pending }
}

/// Spawn the termination watchdog; it records in `stopped` when it fires.
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
    stopped: Arc<Stopped>,
) {
    if let Some(deadline) = timeout {
        let handle = runtime.v8_isolate().thread_safe_handle();
        std::thread::spawn(move || {
            std::thread::sleep(deadline);
            if done.load(Ordering::SeqCst) {
                return;
            }
            stopped.by_timeout.store(true, Ordering::SeqCst);
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
}

/// Drive the event loop and the evaluation future to settlement, with a
/// tokio-side backstop for a fully idle loop the terminator can't reach.
async fn drive(
    runtime: &mut JsRuntime,
    evaluation: impl std::future::Future<Output = Result<(), deno_core::error::CoreError>>,
    timeout: Option<Duration>,
    stopped: &Stopped,
) -> Result<(), HostError> {
    let deadline = timeout.map(|t| t + Duration::from_secs(2));
    let options = PollEventLoopOptions::default();
    let loop_result = match deadline {
        Some(d) => match tokio::time::timeout(d, runtime.run_event_loop(options)).await {
            Ok(r) => r.map_err(|e| classify(&e, stopped)),
            Err(_) => Err(HostError::Terminated(
                "event loop did not settle within deadline".into(),
            )),
        },
        None => runtime
            .run_event_loop(options)
            .await
            .map_err(|e| classify(&e, stopped)),
    };
    let eval_result = evaluation.await.map_err(|e| classify(&e, stopped));
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
fn classify(err: &deno_core::error::CoreError, stopped: &Stopped) -> HostError {
    if stopped.by_heap.load(Ordering::SeqCst) {
        return HostError::Terminated("execution terminated at the V8 heap limit".into());
    }
    if stopped.by_timeout.load(Ordering::SeqCst) {
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
        let (mut runtime, stopped) = super::build_runtime(&options, Some(dir.path()));
        let err = match runtime.load_main_es_module(&specifier).await {
            Err(e) => e,
            Ok(_) => panic!("must fail"),
        };
        let classified = super::classify(&err, &stopped);
        assert!(
            matches!(classified, super::HostError::Syntax(_)),
            "expected Syntax, got {classified:?}"
        );
    }

    /// Request ids are u64 on the wire side and JS numbers in the script.
    /// The respond op used to take a u32, so from request 2^32 on every
    /// answer went to the wrong id and the client waited for nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn a_request_id_past_u32_still_gets_its_answer() {
        use std::io::{Read, Write};
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .unwrap_or_else(|e| panic!("free port: {e}"))
            .port();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let capabilities = crate::Capabilities::from_grants(crate::capability::Grants {
            serve: Some(crate::capability::ServeGrant::new(addr, 1024)),
            ..Default::default()
        });
        let options = super::HostOptions {
            capabilities,
            ..Default::default()
        };
        let (mut runtime, _stopped) = super::build_runtime(&options, None);
        let first = (1u64 << 32) + 5;
        runtime
            .op_state()
            .borrow_mut()
            .put(crate::serve::FirstRequestId(first));

        let client = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut stream = loop {
                match std::net::TcpStream::connect(addr) {
                    Ok(s) => break s,
                    Err(e) if std::time::Instant::now() < deadline => {
                        let _ = e;
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(e) => panic!("listener never came up: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap_or_else(|e| panic!("timeout: {e}"));
            stream
                .write_all(b"GET /id HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                .unwrap_or_else(|e| panic!("write: {e}"));
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            response
        });

        let source = format!(
            "const s = rt.serve({{host: '127.0.0.1', port: {port}}}, async (req) => {{\n\
               rt.setTimeout(() => s.close(), 50);\n\
               return {{status: 200, body: String(req.id)}};\n\
             }});\n"
        );
        let specifier =
            ModuleSpecifier::parse("file:///ids.js").unwrap_or_else(|e| panic!("specifier: {e}"));
        let module = runtime
            .load_main_es_module_from_code(&specifier, source)
            .await
            .unwrap_or_else(|e| panic!("load: {e}"));
        let evaluation = runtime.mod_evaluate(module);
        runtime
            .run_event_loop(Default::default())
            .await
            .unwrap_or_else(|e| panic!("event loop: {e}"));
        evaluation.await.unwrap_or_else(|e| panic!("evaluate: {e}"));
        let response = client.join().unwrap_or_else(|_| panic!("client panicked"));
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "response: {response:?}"
        );
        assert!(
            response.ends_with(&first.to_string()),
            "the script saw id {first} exactly: {response:?}"
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

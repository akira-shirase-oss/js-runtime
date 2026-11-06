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
use deno_core::{
    FsModuleLoader, JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions,
};

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
}

impl Default for HostOptions {
    fn default() -> Self {
        Self { capabilities: Capabilities::deny_all(), timeout: Some(Duration::from_secs(10)) }
    }
}

/// Build a runtime wired with our ops and the given capabilities.
fn build_runtime(options: &HostOptions) -> JsRuntime {
    let runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(Rc::new(FsModuleLoader)),
        extensions: vec![jsrt_core::init()],
        ..Default::default()
    });
    let state = runtime.op_state();
    {
        let mut state = state.borrow_mut();
        state.put(Rc::new(options.capabilities.clone()));
        state.put(Rc::new(reqwest::Client::new()));
    }
    runtime
}

/// Run an ES module file to completion: evaluate it, drive the event loop
/// until pending work settles, and classify every way it can end.
pub async fn run_file(entry: &Path, options: &HostOptions) -> Result<(), HostError> {
    if !entry.is_file() {
        return Err(HostError::Load(format!("no such file: {}", entry.display())));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let specifier = deno_core::resolve_path(&entry.to_string_lossy(), &cwd)
        .map_err(|e| HostError::Load(format!("cannot resolve {}: {e}", entry.display())))?;

    let mut runtime = build_runtime(options);
    let terminated = spawn_watchdog(&mut runtime, options.timeout);

    let module_id = runtime
        .load_main_es_module(&specifier)
        .await
        .map_err(|e| classify(&e, &terminated))?;
    let evaluation = runtime.mod_evaluate(module_id);
    drive(&mut runtime, evaluation, options.timeout, terminated).await
}

/// Run an in-memory module — the test path. Same lifecycle as `run_file`.
pub async fn run_source(name: &str, source: &str, options: &HostOptions) -> Result<(), HostError> {
    let specifier = ModuleSpecifier::parse(&format!("file:///{name}"))
        .map_err(|e| HostError::Load(format!("bad specifier {name}: {e}")))?;
    let mut runtime = build_runtime(options);
    let terminated = spawn_watchdog(&mut runtime, options.timeout);
    let module_id = runtime
        .load_main_es_module_from_code(&specifier, source.to_string())
        .await
        .map_err(|e| classify(&e, &terminated))?;
    let evaluation = runtime.mod_evaluate(module_id);
    drive(&mut runtime, evaluation, options.timeout, terminated).await
}

/// Spawn the termination watchdog and return the flag it sets when it fires.
/// `terminate_execution` is the only mechanism that stops JS *while it runs
/// on this isolate* — a `select!`-style cancel can never see a `while(1){}`.
fn spawn_watchdog(runtime: &mut JsRuntime, timeout: Option<Duration>) -> Arc<AtomicBool> {
    let fired = Arc::new(AtomicBool::new(false));
    if let Some(deadline) = timeout {
        let handle = runtime.v8_isolate().thread_safe_handle();
        let fired_thread = Arc::clone(&fired);
        std::thread::spawn(move || {
            std::thread::sleep(deadline);
            fired_thread.store(true, Ordering::SeqCst);
            handle.terminate_execution();
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
            Err(_) => Err(HostError::Terminated("event loop did not settle within deadline".into())),
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
        CoreErrorKind::ExecutionTerminated => {
            HostError::Terminated("execution terminated".into())
        }
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
            match name {
                "SyntaxError" => HostError::Syntax(detail),
                "CapabilityError" => HostError::Capability(format!("{name}: {detail}")),
                // `detail` already carries the error's own "Error: msg" form.
                _ => HostError::Exception(detail),
            }
        }
        _ => HostError::Exception(err.to_string()),
    }
}

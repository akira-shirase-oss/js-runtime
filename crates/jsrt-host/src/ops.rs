//! The ops — every host capability a script can invoke.
//!
//! An op is the only door out of the isolate. If an ability isn't an op, the
//! script doesn't have it; if it is an op, it checks the capability manifest
//! first. Errors thrown here surface in JS with a `name` scripts can match —
//! `CapabilityError` for refusals, `TypeError`/`Error` for misuse.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use deno_core::{op2, OpState};
use deno_error::JsErrorBox;
use serde::Serialize;

use crate::capability::Capabilities;
use crate::error::HostError;

/// The fetch deadline — a script may not hold a network op open longer.
const FETCH_DEADLINE: Duration = Duration::from_secs(10);
/// Largest response body `rt.fetch` will materialize.
const FETCH_MAX_BODY: u64 = 4 * 1024 * 1024;

/// Errors the host itself produced (I/O failures, deadlines, internal state)
/// surface as class `HostError` so an uncaught one classifies to exit 8 —
/// distinct from a script's own `Error` (exit 5).
fn host_err(msg: String) -> JsErrorBox {
    JsErrorBox::new("HostError", msg)
}

fn deny(err: HostError) -> JsErrorBox {
    match err {
        HostError::Capability(msg) => JsErrorBox::new("CapabilityError", msg),
        HostError::Host(msg) => host_err(msg),
        other => JsErrorBox::generic(other.to_string()),
    }
}

/// Pull the capability manifest out of `OpState` (stored as `Rc<Capabilities>`
/// at runtime build).
fn capabilities(state: &Rc<RefCell<OpState>>) -> Result<Rc<Capabilities>, JsErrorBox> {
    state
        .borrow()
        .try_borrow::<Rc<Capabilities>>()
        .cloned()
        .ok_or_else(|| host_err("capabilities not installed".into()))
}

fn http_client(state: &Rc<RefCell<OpState>>) -> Result<Rc<reqwest::Client>, JsErrorBox> {
    state
        .borrow()
        .try_borrow::<Rc<reqwest::Client>>()
        .cloned()
        .ok_or_else(|| host_err("http client not installed".into()))
}

/// `rt.hostInfo()` — who's hosting this script.
#[derive(Serialize)]
struct HostInfo {
    runtime: &'static str,
    version: &'static str,
    engine: &'static str,
}

#[op2]
#[serde]
fn op_host_info() -> Result<HostInfo, JsErrorBox> {
    Ok(HostInfo {
        runtime: "jsrt",
        version: env!("CARGO_PKG_VERSION"),
        engine: "v8",
    })
}

/// `rt.now()` — wall clock in milliseconds, for script-side measurements
/// (the perf harness times the op boundary with this).
#[op2(fast)]
fn op_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// `rt.sleep(ms)` — the async timer primitive the timer story builds on.
#[op2]
async fn op_sleep(ms: f64) -> Result<(), JsErrorBox> {
    if !ms.is_finite() || ms < 0.0 {
        return Err(JsErrorBox::type_error(
            "rt.sleep: ms must be a non-negative number",
        ));
    }
    tokio::time::sleep(Duration::from_secs_f64(ms / 1000.0)).await;
    Ok(())
}

/// `rt.readText(path)` — capability-gated file read. Throws `CapabilityError`
/// outside declared read paths.
#[op2]
#[string]
async fn op_read_text(
    state: Rc<RefCell<OpState>>,
    #[string] path: String,
) -> Result<String, JsErrorBox> {
    let caps = capabilities(&state)?;
    let resolved = caps.may_read(std::path::Path::new(&path)).map_err(deny)?;
    tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|e| host_err(format!("read {}: {e}", resolved.display())))
}

/// `rt.fetch(url)` — capability-gated, deadline'd, body-capped fetch.
#[derive(Serialize)]
struct FetchResult {
    status: u16,
    #[serde(rename = "statusText")]
    status_text: String,
    body: String,
    bytes: usize,
    #[serde(rename = "durationMs")]
    duration_ms: f64,
}

#[op2]
#[serde]
async fn op_fetch(
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
) -> Result<FetchResult, JsErrorBox> {
    let caps = capabilities(&state)?;
    let client = http_client(&state)?;
    caps.may_net(&url).map_err(deny)?;
    let started = std::time::Instant::now();
    let mut response = tokio::time::timeout(FETCH_DEADLINE, client.get(&url).send())
        .await
        .map_err(|_| host_err(format!("fetch {url}: deadline {FETCH_DEADLINE:?} exceeded")))?
        .map_err(|e| host_err(format!("fetch {url}: {e}")))?;
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| host_err(format!("fetch {url}: body: {e}")))?
    {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > FETCH_MAX_BODY {
            return Err(host_err(format!(
                "fetch {url}: body exceeded {FETCH_MAX_BODY} bytes"
            )));
        }
    }
    Ok(FetchResult {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        body: String::from_utf8_lossy(&body).into_owned(),
        bytes: body.len(),
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

deno_core::extension!(
    jsrt_core,
    ops = [op_host_info, op_now, op_sleep, op_read_text, op_fetch, op_noop],
    esm_entry_point = "ext:jsrt_core/bootstrap.js",
    esm = [dir "js", "bootstrap.js"],
);

/// A no-op used by the perf harness to measure raw op-boundary overhead —
/// wired into the extension but absent from `bootstrap.js`, so scripts see it
/// only via `Deno.core.ops.op_noop` when explicitly told to.
#[op2(fast)]
fn op_noop() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_errors_keep_their_name() {
        use deno_error::JsErrorClass;
        let err = deny(HostError::Capability("read denied: /x".into()));
        assert_eq!(err.get_class(), "CapabilityError");
        assert_eq!(err.get_message().as_ref(), "read denied: /x");
    }
}

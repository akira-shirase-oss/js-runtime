//! `rt.serve` — a capability-gated HTTP listener.
//!
//! The model is pull, not push: the script's event loop drives dispatch
//! (`await op_serve_next` → handler → `op_serve_respond`), so a crashing
//! handler is a JavaScript exception inside a loop the host already owns —
//! it can never take the host down, because the host was never inside the
//! handler. hyper owns the wire: keep-alive, request-line parsing, partial
//! reads and backpressure are its problem, tested under real load.
//!
//! Bounds: request line+headers capped by hyper's read buffer (16 KiB),
//! bodies capped by the grant's `max_body` (over-limit → 413 before the
//! script ever sees the request), in-flight requests capped by the
//! dispatch channel (64) — past that, TCP backpressure is the honest
//! answer, not an unbounded queue.
//!
//! Loopback-only is not a DoS bound — anything on the box can connect — so
//! the grant also carries: `max_connections` (an owned semaphore permit is
//! taken *before* `accept`, so connections past the cap wait in the
//! kernel backlog rather than as tasks here), `header_timeout` (hyper's
//! header-read timer, which also runs while a keep-alive connection sits
//! idle waiting for its next head), and `body_timeout` (408 for a body
//! that stalls).
//!
//! A body is read *before* the request takes a dispatch-queue slot, so a
//! stalled upload never sits in the queue ahead of other requests (taking
//! the slot first let 64 half-sent bodies block every GET for the whole
//! body timeout). Memory for bodies is bounded separately: every body
//! byte held in this process — being read or waiting in the queue —
//! holds a permit from one per-listener byte budget
//! ([`BODY_BUFFER_BUDGET`], raised to `max_body` if that is larger),
//! taken chunk by chunk as the bytes arrive. To hold the budget a client
//! has to actually send the bytes, and the body timeout still ends it.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deno_core::{op2, OpState, Resource, ResourceId};
use deno_error::JsErrorBox;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex, OwnedSemaphorePermit, Semaphore};

use crate::capability::ServeGrant;

/// In-flight requests the JS side hasn't picked up yet. Past this, new
/// connections stall at the socket — the backlog lives in TCP, not here.
const DISPATCH_QUEUE: usize = 64;
/// hyper's read buffer: bounds request-line + header bytes.
const HEAD_BUF_MAX: usize = 16 * 1024;
/// Header count cap — past this the request is refused before dispatch.
const MAX_HEADERS: usize = 64;
/// Request-body bytes one listener holds at once across all connections,
/// counting bodies being read and bodies waiting for the script.
const BODY_BUFFER_BUDGET: u64 = 64 * 1024 * 1024;

type PendingResponse = oneshot::Sender<Response<Full<Bytes>>>;

/// One dispatched request: the wire-facing half lives in hyper's task,
/// the script-facing half goes to `op_serve_next`.
struct ServeReq {
    id: u64,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
    /// This body's share of the listener's byte budget, released when the
    /// script takes the request.
    _body_budget: Option<OwnedSemaphorePermit>,
}

/// The listener's shared state — held in the resource table so runtime
/// teardown closes it like any other resource.
pub struct ServeResource {
    rx: TokioMutex<mpsc::Receiver<(ServeReq, PendingResponse)>>,
    pending: Mutex<HashMap<u64, PendingResponse>>,
    next_id: AtomicU64,
    accept_task: Mutex<Option<tokio::task::AbortHandle>>,
}

impl Resource for ServeResource {
    fn name(&self) -> Cow<'_, str> {
        "jsrt.serve".into()
    }

    fn close(self: Rc<Self>) {
        if let Ok(mut guard) = self.accept_task.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
        // Dropping `pending` drops every waiting sender — in-flight
        // hyper tasks see a closed oneshot and answer 503 themselves.
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
    }
}

fn grant(state: &Rc<RefCell<OpState>>) -> Result<Rc<ServeGrant>, JsErrorBox> {
    state
        .borrow()
        .try_borrow::<Rc<ServeGrant>>()
        .cloned()
        .ok_or_else(|| {
            JsErrorBox::new(
                "CapabilityError",
                "serve denied: no listener declared in the manifest".to_string(),
            )
        })
}

fn host_err(msg: String) -> JsErrorBox {
    JsErrorBox::new("HostError", msg)
}

/// The first request id a listener hands out (default 1). An embedder
/// knob that lets tests start past `u32::MAX` without sending four billion
/// requests; scripts cannot reach `OpState`.
#[derive(Debug, Clone, Copy)]
pub struct FirstRequestId(pub u64);

/// Largest integer a JS number carries exactly — ids beyond it would be
/// rounded on their way through the script.
const MAX_SAFE_ID: f64 = 9_007_199_254_740_991.0;

/// A request id back from the script: a JS number holding a whole u64.
fn request_id(raw: f64) -> Result<u64, JsErrorBox> {
    if raw.is_finite() && raw >= 1.0 && raw.fract() == 0.0 && raw <= MAX_SAFE_ID {
        Ok(raw as u64)
    } else {
        Err(JsErrorBox::type_error(format!(
            "rt.serve: {raw} is not a request id"
        )))
    }
}

/// Build the wire response, or say why the handler's answer can't be one.
fn build_response(
    status: f64,
    headers: Vec<(String, String)>,
    body: String,
) -> Result<Response<Full<Bytes>>, String> {
    // 1xx cannot be a final answer and hyper would mis-frame it; 6xx-9xx
    // are not HTTP. Fractions, NaN and out-of-range numbers fail here too.
    if !(status.fract() == 0.0 && (200.0..=599.0).contains(&status)) {
        return Err(format!("status {status} is not an HTTP status in 200-599"));
    }
    let mut builder = Response::builder().status(status as u16);
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| format!("invalid response: {e}"))
}

fn internal_error() -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(b"invalid handler response")));
    *response.status_mut() = http::StatusCode::INTERNAL_SERVER_ERROR;
    response
}

/// `rt.serve` step one: bind the declared address, spawn the accept loop,
/// return the resource id. Sync — `TcpListener::bind` is a syscall, not
/// worth an await.
#[op2(fast)]
fn op_serve_listen(
    state: Rc<RefCell<OpState>>,
    #[string] host: String,
    port: u32,
) -> Result<u32, JsErrorBox> {
    let grant = grant(&state)?;
    let ip: std::net::IpAddr = host
        .parse()
        .map_err(|_| JsErrorBox::type_error("rt.serve: host must be an IP literal"))?;
    let addr = std::net::SocketAddr::new(ip, port as u16);
    if addr != grant.addr {
        return Err(JsErrorBox::new(
            "CapabilityError",
            format!(
                "serve denied: {addr} is not the declared listener {}",
                grant.addr
            ),
        ));
    }

    // std bind + set_nonblocking keeps this op synchronous; the port-in-use
    // answer is a host error, not a capability one.
    let std_listener = std::net::TcpListener::bind(addr)
        .map_err(|e| host_err(format!("serve listen {addr}: {e}")))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| host_err(format!("serve listen {addr}: {e}")))?;
    let listener = tokio::net::TcpListener::from_std(std_listener)
        .map_err(|e| host_err(format!("serve listen {addr}: {e}")))?;

    let (tx, rx) = mpsc::channel(DISPATCH_QUEUE);
    let resource = Rc::new(ServeResource {
        rx: TokioMutex::new(rx),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(
            state
                .borrow()
                .try_borrow::<FirstRequestId>()
                .map_or(1, |first| first.0),
        ),
        accept_task: Mutex::new(None),
    });

    // At least one full-size body must fit, and tokio caps a semaphore.
    let budget = grant
        .max_body
        .max(BODY_BUFFER_BUDGET)
        .min(Semaphore::MAX_PERMITS as u64);
    let limits = WireLimits {
        max_body: grant.max_body,
        body_timeout: grant.body_timeout,
        body_budget: Arc::new(Semaphore::new(
            usize::try_from(budget).unwrap_or(Semaphore::MAX_PERMITS),
        )),
    };
    let header_timeout = grant.header_timeout;
    let slots = Arc::new(Semaphore::new(grant.max_connections));
    let task_resource = resource.clone();
    let task = deno_core::unsync::spawn(async move {
        loop {
            // A slot first, then accept: past the cap, connections queue in
            // the kernel backlog, not as tasks and buffers in this process.
            let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                return;
            };
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = tx.clone();
                    let res = task_resource.clone();
                    let limits = limits.clone();
                    deno_core::unsync::spawn(async move {
                        // Held for the connection's whole life.
                        let _slot = slot;
                        let svc = service_fn(move |req: Request<Incoming>| {
                            dispatch(req, tx.clone(), res.clone(), limits.clone())
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .max_buf_size(HEAD_BUF_MAX)
                            .timer(TokioTimer::new())
                            .header_read_timeout(header_timeout)
                            .serve_connection(TokioIo::new(stream), svc)
                            .await;
                    });
                }
                Err(_) => {
                    drop(slot);
                    // EMFILE bursts would spin — a beat of backoff keeps an
                    // fd-exhausted box from becoming a CPU problem too.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });
    if let Ok(mut guard) = resource.accept_task.lock() {
        *guard = Some(task.abort_handle());
    }

    let rid = state.borrow_mut().resource_table.add_rc(resource);
    Ok(rid)
}

/// Per-request bounds the wire side enforces before the script is involved.
#[derive(Clone)]
struct WireLimits {
    max_body: u64,
    body_timeout: Duration,
    /// The listener's body byte budget, shared by every connection.
    body_budget: Arc<Semaphore>,
}

/// Why a body could not be collected.
enum BodyFailure {
    TooLarge,
    Unreadable,
}

/// Collect a body, taking a budget permit for each chunk as it arrives —
/// the budget counts bytes actually received, so declaring a large
/// `Content-Length` and sending two bytes holds two bytes of it.
async fn read_body(
    mut body: Incoming,
    max_body: u64,
    budget: &Arc<Semaphore>,
) -> Result<(Vec<u8>, Option<OwnedSemaphorePermit>), BodyFailure> {
    let mut buf = Vec::new();
    let mut held: Option<OwnedSemaphorePermit> = None;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| BodyFailure::Unreadable)?;
        let Ok(data) = frame.into_data() else {
            continue; // trailers carry no body bytes
        };
        if data.is_empty() {
            continue;
        }
        if buf.len() as u64 + data.len() as u64 > max_body {
            return Err(BodyFailure::TooLarge);
        }
        let n = u32::try_from(data.len()).map_err(|_| BodyFailure::TooLarge)?;
        let permit = Arc::clone(budget)
            .acquire_many_owned(n)
            .await
            .map_err(|_| BodyFailure::Unreadable)?;
        match &mut held {
            Some(h) => h.merge(permit),
            None => held = Some(permit),
        }
        buf.extend_from_slice(&data);
    }
    Ok((buf, held))
}

/// Wire-side handler: collect the bounded body, take a dispatch slot, hand
/// the request to the script, await its response. Every failure mode maps
/// to a real HTTP answer — the wire never sees a Rust panic.
async fn dispatch(
    req: Request<Incoming>,
    tx: mpsc::Sender<(ServeReq, PendingResponse)>,
    res: Rc<ServeResource>,
    limits: WireLimits,
) -> Result<Response<Full<Bytes>>, Infallible> {
    fn plain(status: u16, msg: &'static str) -> Response<Full<Bytes>> {
        Response::builder()
            .status(status)
            .body(Full::new(Bytes::from_static(msg.as_bytes())))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
    }

    let (parts, body) = req.into_parts();
    if parts.headers.len() > MAX_HEADERS {
        return Ok(plain(431, "too many headers"));
    }
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    // A declared length over the cap is refused before anything is read.
    let declared = parts
        .headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > limits.max_body) {
        return Ok(plain(413, "request body too large"));
    }

    // The body first, under its own deadline and the shared byte budget;
    // only a complete request competes for a dispatch-queue slot, so a
    // stalled upload can never stand in front of a GET.
    let collected = tokio::time::timeout(
        limits.body_timeout,
        read_body(body, limits.max_body, &limits.body_budget),
    )
    .await;
    let (bytes, body_budget) = match collected {
        Ok(Ok(read)) => read,
        Ok(Err(BodyFailure::TooLarge)) => return Ok(plain(413, "request body too large")),
        Ok(Err(BodyFailure::Unreadable)) => return Ok(plain(400, "request body unreadable")),
        Err(_) => return Ok(plain(408, "request body timeout")),
    };

    let id = res.next_id.fetch_add(1, Ordering::Relaxed);
    let (rtx, rrx) = oneshot::channel();
    let msg = ServeReq {
        id,
        method: parts.method.to_string(),
        path,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
        _body_budget: body_budget,
    };
    drop(bytes);
    // The queue being full means the script is busy: backpressure is this
    // await. The sender travels with the message; if the receiver is gone,
    // the script's event loop is done and so is this connection.
    if tx.send((msg, rtx)).await.is_err() {
        return Ok(plain(503, "listener closing"));
    }
    match rrx.await {
        Ok(response) => Ok(response),
        // The resource closed (or the script's isolate died) while this
        // request was in flight — honest 503, not a hang.
        Err(_) => Ok(plain(503, "host gone")),
    }
}

/// `await op_serve_next(rid)` — the next dispatched request, or null when
/// the listener closed.
#[derive(Serialize)]
struct ServeNext {
    id: u64,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

#[op2]
#[serde]
async fn op_serve_next(
    state: Rc<RefCell<OpState>>,
    rid: ResourceId,
) -> Result<serde_json::Value, JsErrorBox> {
    let res = state
        .borrow()
        .resource_table
        .get::<ServeResource>(rid)
        .map_err(|e| host_err(format!("serve next: {e}")))?;
    let msg = {
        let mut rx = res.rx.lock().await;
        rx.recv().await
    };
    match msg {
        Some((req, sender)) => {
            if let Ok(mut pending) = res.pending.lock() {
                pending.insert(req.id, sender);
            }
            Ok(serde_json::json!(ServeNext {
                id: req.id,
                method: req.method,
                path: req.path,
                headers: req.headers,
                body: req.body,
            }))
        }
        None => Ok(serde_json::Value::Null),
    }
}

/// `op_serve_respond(rid, reqId, status, headers, body)` — write the
/// script's answer back over the wire. Unknown ids (client hung up first,
/// or already answered) and a closed listener (its close answered every
/// in-flight request 503) are a quiet no-op. An answer that can't be an
/// HTTP response is sent as a 500 *and* reported to the script as a
/// TypeError — the client is never left with an empty 200 or no answer.
///
/// Ids travel as JS numbers (f64): exact up to 2^53, where a `u32`
/// parameter used to wrap them after 2^32 requests and drop the answer.
#[op2]
fn op_serve_respond(
    state: Rc<RefCell<OpState>>,
    rid: ResourceId,
    req_id: f64,
    status: f64,
    #[serde] headers: Vec<(String, String)>,
    #[string] body: String,
) -> Result<(), JsErrorBox> {
    let id = request_id(req_id)?;
    let Ok(res) = state.borrow().resource_table.get::<ServeResource>(rid) else {
        return Ok(());
    };
    let sender = res
        .pending
        .lock()
        .ok()
        .and_then(|mut pending| pending.remove(&id));
    let Some(sender) = sender else {
        return Ok(());
    };
    // If the client already hung up, send fails — fine either way.
    match build_response(status, headers, body) {
        Ok(response) => {
            let _ = sender.send(response);
            Ok(())
        }
        Err(why) => {
            let _ = sender.send(internal_error());
            Err(JsErrorBox::type_error(format!("rt.serve: {why}")))
        }
    }
}

/// `op_serve_close(rid)` — take the resource out of the table; its close
/// aborts the accept task and answers in-flight requests 503.
#[op2]
fn op_serve_close(state: Rc<RefCell<OpState>>, rid: ResourceId) -> Result<(), JsErrorBox> {
    state
        .borrow_mut()
        .resource_table
        .take::<ServeResource>(rid)
        .map_err(|e| host_err(format!("serve close: {e}")))?
        .close();
    Ok(())
}

deno_core::extension!(
    jsrt_serve,
    ops = [
        op_serve_listen,
        op_serve_next,
        op_serve_respond,
        op_serve_close
    ],
);

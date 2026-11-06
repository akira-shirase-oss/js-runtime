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

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use deno_core::{op2, OpState, Resource, ResourceId};
use deno_error::JsErrorBox;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex};

use crate::capability::ServeGrant;

/// In-flight requests the JS side hasn't picked up yet. Past this, new
/// connections stall at the socket — the backlog lives in TCP, not here.
const DISPATCH_QUEUE: usize = 64;
/// hyper's read buffer: bounds request-line + header bytes.
const HEAD_BUF_MAX: usize = 16 * 1024;
/// Header count cap — past this the request is refused before dispatch.
const MAX_HEADERS: usize = 64;

type PendingResponse = oneshot::Sender<Response<Full<Bytes>>>;

/// One dispatched request: the wire-facing half lives in hyper's task,
/// the script-facing half goes to `op_serve_next`.
struct ServeReq {
    id: u64,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
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
        next_id: AtomicU64::new(1),
        accept_task: Mutex::new(None),
    });

    let max_body = grant.max_body;
    let task_resource = resource.clone();
    let task = deno_core::unsync::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = tx.clone();
                    let res = task_resource.clone();
                    deno_core::unsync::spawn(async move {
                        let svc = service_fn(move |req: Request<Incoming>| {
                            dispatch(req, tx.clone(), res.clone(), max_body)
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .max_buf_size(HEAD_BUF_MAX)
                            .serve_connection(TokioIo::new(stream), svc)
                            .await;
                    });
                }
                Err(_) => {
                    // EMFILE bursts would spin — a beat of backoff keeps an
                    // fd-exhausted box from becoming a CPU problem too.
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
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

/// Wire-side handler: collect the bounded body, hand the request to the
/// dispatch queue, await the script's response. Every failure mode maps
/// to a real HTTP answer — the wire never sees a Rust panic.
async fn dispatch(
    req: Request<Incoming>,
    tx: mpsc::Sender<(ServeReq, PendingResponse)>,
    res: Rc<ServeResource>,
    max_body: u64,
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

    let collected = Limited::new(body, max_body as usize).collect().await;
    let bytes = match collected {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(plain(413, "request body too large")),
    };

    let id = res.next_id.fetch_add(1, Ordering::Relaxed);
    let (rtx, rrx) = oneshot::channel();
    let msg = ServeReq {
        id,
        method: parts.method.to_string(),
        path,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    };
    // The queue being full means the script is busy — backpressure lives
    // here, in the send() await, not in an unbounded Vec. The sender
    // travels with the message; if the receiver is gone, the script's
    // event loop is done and so is this connection.
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
/// script's answer back over the wire. Unknown ids (client hung up first)
/// are a quiet no-op, not an error.
#[op2]
fn op_serve_respond(
    state: Rc<RefCell<OpState>>,
    rid: ResourceId,
    req_id: u32,
    status: u32,
    #[serde] headers: Vec<(String, String)>,
    #[string] body: String,
) -> Result<(), JsErrorBox> {
    let res = state
        .borrow()
        .resource_table
        .get::<ServeResource>(rid)
        .map_err(|e| host_err(format!("serve respond: {e}")))?;
    let sender = res
        .pending
        .lock()
        .ok()
        .and_then(|mut pending| pending.remove(&u64::from(req_id)));
    if let Some(sender) = sender {
        let mut builder = Response::builder().status(status as u16);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        let response = builder
            .body(Full::new(Bytes::from(body)))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));
        // If the client already hung up, send fails — fine.
        let _ = sender.send(response);
    }
    Ok(())
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

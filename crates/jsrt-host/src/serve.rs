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
//! idle waiting for its next head), `body_timeout` (408 for a body
//! that stalls) and `write_timeout` (a response write that makes no
//! progress for that long drops the connection — without it, a client
//! that requests a large answer and never reads would hold its slot
//! forever, and `max_connections` such clients would stop the listener).
//!
//! A body is read *before* the request takes a dispatch-queue slot, so a
//! stalled upload never sits in the queue ahead of other requests (taking
//! the slot first let 64 half-sent bodies block every GET for the whole
//! body timeout). Memory for bodies is bounded separately, and the rule
//! is that a request never waits for budget while it holds some (taking
//! the budget chunk by chunk and waiting for more let concurrent uploads
//! each hold part of it and none finish — a deadlock ended only by 408s):
//! - the first [`FREE_BODY_BYTES`] (64 KiB) of every body are outside the
//!   budget — bounded by `max_connections` x 64 KiB — so small requests
//!   (webhooks, forms, JSON) never wait on, or fail for, the budget;
//! - a body that declares a larger `Content-Length` reserves all of it
//!   from the per-listener budget ([`BODY_BUFFER_BUDGET`], raised to
//!   `max_body` if that is larger) before a byte is read, or is answered
//!   503 with `Retry-After` at once; with `Expect: 100-continue` the client
//!   learns before sending the body;
//! - a chunked body takes budget for each chunk past the free share
//!   without waiting; when none is left it lets go of what it holds and
//!   is answered 503 with `Retry-After`.
//!
//! A body that holds budget has to earn it: from the moment it takes
//! some, it gets `body_first_byte` to send a byte and must then average
//! `body_min_rate`, or it is answered 408 and lets go. A body inside the
//! free share holds no budget, only its connection slot, so only
//! `body_timeout` bounds it. The cost: a client sending at the minimum
//! rate keeps up to `max_body` of the budget per connection until
//! `body_timeout`. Other large uploads get fast 503s meanwhile; small
//! ones are unaffected.
//!
//! At the connection cap a newcomer closes the connection that has been
//! in its current phase longest — waiting for a head, or reading a body —
//! never one with a request in the script or a response being written.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use deno_core::{op2, OpState, Resource, ResourceId};
use deno_error::JsErrorBox;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
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
/// The share of every body that is outside the budget.
const FREE_BODY_BYTES: u64 = 64 * 1024;
/// What a 503 for a full budget tells the client to wait, in seconds.
const BUSY_RETRY_AFTER_SECS: &str = "1";
/// How long a refused upload's body is read and thrown away before the
/// refusal is sent, so a client without `Expect: 100-continue` finishes
/// writing and reads the answer instead of hitting a reset.
const REFUSAL_DRAIN: Duration = Duration::from_millis(200);

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
    /// Set once by `close`. The pending `op_serve_next` watches it, so the
    /// script's loop ends even while connections (each holding a sender
    /// of the dispatch queue) stay open; dispatch checks it so a request
    /// finishing after close is answered 503 instead of queued forever.
    closed: tokio::sync::watch::Sender<bool>,
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
        self.closed.send_replace(true);
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
        closed: tokio::sync::watch::Sender::new(false),
    });

    // At least one full-size body must fit, and tokio caps a semaphore.
    let budget = grant
        .max_body
        .max(BODY_BUFFER_BUDGET)
        .min(Semaphore::MAX_PERMITS as u64);
    let limits = WireLimits {
        max_body: grant.max_body,
        body_timeout: grant.body_timeout,
        body_first_byte: grant.body_first_byte,
        body_min_rate: grant.body_min_rate,
        body_budget: Arc::new(Semaphore::new(
            usize::try_from(budget).unwrap_or(Semaphore::MAX_PERMITS),
        )),
    };
    let header_timeout = grant.header_timeout;
    let write_timeout = grant.write_timeout;
    let slots = Arc::new(Semaphore::new(grant.max_connections));
    let task_resource = resource.clone();
    let task = deno_core::unsync::spawn(async move {
        let conns = Connections::default();
        loop {
            // Below the cap: a slot first, then accept. At the cap: wait for
            // a client to actually arrive, then close the connection that
            // has spent longest in its current phase (waiting for a head,
            // or reading a body) to make room — never one the script is
            // handling or whose response is being written. Without that,
            // max_connections idle or slow sockets held every slot for the
            // header/body timeouts and the attacker just reconnected. If
            // every connection is protected, the newcomer waits for a slot
            // as before.
            let (slot, accepted) = match Arc::clone(&slots).try_acquire_owned() {
                Ok(slot) => (slot, listener.accept().await),
                Err(_) => {
                    let accepted = listener.accept().await;
                    if accepted.is_ok() {
                        conns.evict_longest_in_phase();
                    }
                    let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                        return;
                    };
                    (slot, accepted)
                }
            };
            match accepted {
                Ok((stream, _)) => {
                    let tx = tx.clone();
                    let res = task_resource.clone();
                    let limits = limits.clone();
                    let (track, evicted) = conns.register();
                    let conns = conns.clone();
                    deno_core::unsync::spawn(async move {
                        // Held for the connection's whole life.
                        let _slot = slot;
                        let svc_track = Rc::clone(&track);
                        let svc = service_fn(move |req: Request<Incoming>| {
                            let track = Rc::clone(&svc_track);
                            let answer = dispatch(
                                req,
                                tx.clone(),
                                res.clone(),
                                limits.clone(),
                                Rc::clone(&svc_track),
                            );
                            async move {
                                let answer = answer.await;
                                // hyper writes the answer next; until that
                                // write is flushed the connection is not
                                // evictable (StallGuard::poll_flush clears
                                // it, write_timeout bounds it).
                                track.writing.set(true);
                                answer
                            }
                        });
                        let io =
                            TokioIo::new(StallGuard::new(stream, write_timeout, Rc::clone(&track)));
                        let serving = hyper::server::conn::http1::Builder::new()
                            .max_buf_size(HEAD_BUF_MAX)
                            .timer(TokioTimer::new())
                            .header_read_timeout(header_timeout)
                            .serve_connection(io, svc);
                        // Dropping the connection future closes the socket.
                        tokio::select! {
                            _ = serving => {}
                            _ = evicted => {}
                        }
                        conns.forget(track.id);
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

/// What the accept loop knows about one live connection.
struct ConnTrack {
    id: u64,
    /// When the connection's current phase began: it connected, or a
    /// response finished and it is waiting for the next head, or a head
    /// arrived and its body is being read. A head or a body should arrive
    /// in one burst, so time spent in a phase is what a client holding a
    /// slot cannot hide; the time of its last byte is not, since a
    /// trickler sends one byte at a time to look busy.
    phase_since: std::cell::Cell<tokio::time::Instant>,
    /// Requests on this connection the script is handling right now —
    /// such a connection is never evicted.
    in_script: std::cell::Cell<u32>,
    /// A response is being written and is not yet flushed — such a
    /// connection is not evicted either (a client that stops reading is
    /// dropped by `write_timeout` instead).
    writing: std::cell::Cell<bool>,
    /// Fired to close the connection when it is evicted.
    evict: RefCell<Option<oneshot::Sender<()>>>,
}

/// Live connections of one listener, for eviction at the cap. Single
/// threaded: everything here runs on the script's executor.
#[derive(Clone, Default)]
struct Connections {
    live: Rc<RefCell<HashMap<u64, Rc<ConnTrack>>>>,
    next_id: Rc<std::cell::Cell<u64>>,
}

impl Connections {
    fn register(&self) -> (Rc<ConnTrack>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let track = Rc::new(ConnTrack {
            id,
            phase_since: std::cell::Cell::new(tokio::time::Instant::now()),
            in_script: std::cell::Cell::new(0),
            writing: std::cell::Cell::new(false),
            evict: RefCell::new(Some(tx)),
        });
        self.live.borrow_mut().insert(id, Rc::clone(&track));
        (track, rx)
    }

    fn forget(&self, id: u64) {
        self.live.borrow_mut().remove(&id);
    }

    /// Close the connection that has been in its current phase longest,
    /// among those with no request in the script and no response being
    /// written. Returns whether one was found.
    fn evict_longest_in_phase(&self) -> bool {
        let victim = self
            .live
            .borrow()
            .values()
            .filter(|c| c.in_script.get() == 0 && !c.writing.get() && c.evict.borrow().is_some())
            .min_by_key(|c| c.phase_since.get())
            .cloned();
        match victim {
            Some(conn) => {
                if let Some(tx) = conn.evict.borrow_mut().take() {
                    let _ = tx.send(());
                }
                true
            }
            None => false,
        }
    }
}

/// Marks a request as being in the script for as long as it lives.
struct InScript(Rc<ConnTrack>);

impl InScript {
    fn new(track: Rc<ConnTrack>) -> Self {
        track.in_script.set(track.in_script.get() + 1);
        Self(track)
    }
}

impl Drop for InScript {
    fn drop(&mut self) {
        self.0
            .in_script
            .set(self.0.in_script.get().saturating_sub(1));
    }
}

/// A socket whose writes give up when they make no progress for `limit`.
/// hyper's header timer only runs while it reads a head; once a response
/// is being written, a peer that stops reading would park the write (and
/// the connection's slot) forever. The clock starts at the first write
/// that cannot proceed and resets whenever bytes move.
struct StallGuard<T> {
    inner: T,
    limit: Duration,
    stalled: Option<Pin<Box<tokio::time::Sleep>>>,
    /// A completed flush ends a response's write: the connection is
    /// evictable again and starts waiting for its next head.
    track: Rc<ConnTrack>,
}

impl<T> StallGuard<T> {
    fn new(inner: T, limit: Duration, track: Rc<ConnTrack>) -> Self {
        Self {
            inner,
            limit,
            stalled: None,
            track,
        }
    }

    /// Map a pending write-side poll onto the stall clock.
    fn watch<R>(&mut self, cx: &mut Context<'_>, poll: Poll<io::Result<R>>) -> Poll<io::Result<R>> {
        match poll {
            Poll::Ready(r) => {
                self.stalled = None;
                Poll::Ready(r)
            }
            Poll::Pending => {
                let limit = self.limit;
                let timer = self
                    .stalled
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
                if timer.as_mut().poll(cx).is_ready() {
                    self.stalled = None;
                    Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "client stopped reading the response",
                    )))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for StallGuard<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for StallGuard<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.watch(cx, poll)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.watch(cx, poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_flush(cx);
        // hyper flushes the socket only once its own write buffer is empty,
        // so a completed flush means the response is out.
        if matches!(poll, Poll::Ready(Ok(()))) && self.track.writing.replace(false) {
            self.track.phase_since.set(tokio::time::Instant::now());
        }
        self.watch(cx, poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.watch(cx, poll)
    }
}

/// Per-request bounds the wire side enforces before the script is involved.
#[derive(Clone)]
struct WireLimits {
    max_body: u64,
    body_timeout: Duration,
    body_first_byte: Duration,
    body_min_rate: u64,
    /// The listener's body byte budget, shared by every connection.
    body_budget: Arc<Semaphore>,
}

/// Why a body could not be collected.
enum BodyFailure {
    TooLarge,
    Unreadable,
    /// The body budget is full; the request held nothing when it said so.
    Busy,
    /// The body did not start within its first-byte allowance, or fell
    /// below the minimum rate after it.
    TooSlow,
}

/// Take `n` bytes of budget now or not at all.
fn take_budget(budget: &Arc<Semaphore>, n: u64) -> Result<OwnedSemaphorePermit, BodyFailure> {
    let n = u32::try_from(n).map_err(|_| BodyFailure::TooLarge)?;
    Arc::clone(budget)
        .try_acquire_many_owned(n)
        .map_err(|_| BodyFailure::Busy)
}

/// Collect a body under the budget rule in the module docs: a declared
/// length past the free share is reserved whole before anything is read;
/// a chunked body takes budget per chunk past the free share. Nothing here
/// ever waits for budget, so no request can hold some while waiting for
/// more.
async fn read_body(
    body: &mut Incoming,
    declared: Option<u64>,
    limits: &WireLimits,
) -> Result<(Vec<u8>, Option<OwnedSemaphorePermit>), BodyFailure> {
    let max_body = limits.max_body;
    let budget = &limits.body_budget;
    let mut held: Option<OwnedSemaphorePermit> = match declared {
        Some(n) if n > FREE_BODY_BYTES => Some(take_budget(budget, n)?),
        _ => None,
    };
    // Grown as bytes arrive: a declared length is a promise, not memory.
    let mut buf = Vec::with_capacity(
        usize::try_from(declared.map_or(0, |n| n.min(FREE_BODY_BYTES))).unwrap_or(0),
    );
    // A reservation has to be earned: from when the body first holds
    // budget, the first byte within the first-byte allowance, then on
    // average at least `body_min_rate` bytes a second. Falling behind is a
    // 408, and dropping `held` gives the budget back. A body that holds no
    // budget (within the free share) costs only its connection slot, which
    // eviction can reclaim, so only `body_timeout` (the caller's) bounds it.
    // The clock: when the budget was taken, and the bytes held by then.
    let mut rate_clock = held.as_ref().map(|_| (tokio::time::Instant::now(), 0usize));
    loop {
        let next = match rate_clock {
            Some((since, base)) => {
                let earned = Duration::try_from_secs_f64(
                    (buf.len() - base) as f64 / limits.body_min_rate as f64,
                )
                .unwrap_or(Duration::MAX);
                let deadline = since
                    .checked_add(limits.body_first_byte.saturating_add(earned))
                    .unwrap_or_else(|| since + limits.body_timeout);
                tokio::time::timeout_at(deadline, body.frame()).await
            }
            None => Ok(body.frame().await),
        };
        let frame = match next {
            Err(_) => return Err(BodyFailure::TooSlow),
            Ok(None) => break,
            Ok(Some(frame)) => frame,
        };
        let frame = frame.map_err(|_| BodyFailure::Unreadable)?;
        let Ok(data) = frame.into_data() else {
            continue; // trailers carry no body bytes
        };
        if data.is_empty() {
            continue;
        }
        let total = buf.len() as u64 + data.len() as u64;
        if total > max_body {
            return Err(BodyFailure::TooLarge);
        }
        if declared.is_none() {
            // Chunked: past the free share, every byte needs budget —
            // taken now, or the whole request lets go and is refused.
            let needed = total.saturating_sub(FREE_BODY_BYTES);
            let have = held.as_ref().map_or(0, |p| p.num_permits() as u64);
            if needed > have {
                let more = take_budget(budget, needed - have)?;
                match &mut held {
                    Some(h) => h.merge(more),
                    None => {
                        held = Some(more);
                        rate_clock = Some((tokio::time::Instant::now(), buf.len()));
                    }
                }
            }
        }
        buf.extend_from_slice(&data);
    }
    Ok((buf, held))
}

/// Read and discard what is left of a refused upload, for at most
/// [`REFUSAL_DRAIN`] and `max_bytes`, so the client that is still sending
/// gets to read the answer. Skipped for `Expect: 100-continue` requests:
/// polling their body would invite the body the refusal is meant to stop.
async fn drain_refused(body: &mut Incoming, expects_continue: bool, max_bytes: u64) {
    if expects_continue {
        return;
    }
    let _ = tokio::time::timeout(REFUSAL_DRAIN, async {
        let mut seen: u64 = 0;
        while let Some(Ok(frame)) = body.frame().await {
            if let Ok(data) = frame.into_data() {
                seen += data.len() as u64;
                if seen > max_bytes {
                    return;
                }
            }
        }
    })
    .await;
}

/// Wire-side handler: collect the bounded body, take a dispatch slot, hand
/// the request to the script, await its response. Every failure mode maps
/// to a real HTTP answer — the wire never sees a Rust panic.
async fn dispatch(
    req: Request<Incoming>,
    tx: mpsc::Sender<(ServeReq, PendingResponse)>,
    res: Rc<ServeResource>,
    limits: WireLimits,
    track: Rc<ConnTrack>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    fn plain(status: u16, msg: &'static str) -> Response<Full<Bytes>> {
        Response::builder()
            .status(status)
            .body(Full::new(Bytes::from_static(msg.as_bytes())))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
    }
    fn busy() -> Response<Full<Bytes>> {
        Response::builder()
            .status(503)
            .header(http::header::RETRY_AFTER, BUSY_RETRY_AFTER_SECS)
            .header(http::header::CONNECTION, "close")
            .body(Full::new(Bytes::from_static(b"request body budget full")))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
    }

    // A head arrived: the connection's body phase starts now.
    track.phase_since.set(tokio::time::Instant::now());
    let (parts, mut body) = req.into_parts();
    let expects_continue = parts
        .headers
        .get(http::header::EXPECT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
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
        drain_refused(&mut body, expects_continue, limits.max_body).await;
        return Ok(plain(413, "request body too large"));
    }

    // The body first, under its own deadline and the shared byte budget;
    // only a complete request competes for a dispatch-queue slot, so a
    // stalled upload can never stand in front of a GET.
    let collected =
        tokio::time::timeout(limits.body_timeout, read_body(&mut body, declared, &limits)).await;
    let (bytes, body_budget) = match collected {
        Ok(Ok(read)) => read,
        Ok(Err(BodyFailure::TooLarge)) => {
            drain_refused(&mut body, expects_continue, limits.max_body).await;
            return Ok(plain(413, "request body too large"));
        }
        Ok(Err(BodyFailure::Unreadable)) => return Ok(plain(400, "request body unreadable")),
        Ok(Err(BodyFailure::Busy)) => {
            drain_refused(&mut body, expects_continue, limits.max_body).await;
            return Ok(busy());
        }
        Ok(Err(BodyFailure::TooSlow)) => return Ok(plain(408, "request body too slow")),
        Err(_) => return Ok(plain(408, "request body timeout")),
    };

    // From here until the answer is built the request belongs to the
    // script, and its connection is not a candidate for eviction.
    let _in_script = InScript::new(track);
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
    // await. The sender travels with the message; if the listener closed
    // or the receiver is gone, nobody will take the request.
    if *res.closed.borrow() || tx.send((msg, rtx)).await.is_err() {
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
    // A listener closed before this call (close() inside the handler) is
    // the same as one that closes while waiting: no more requests.
    let Ok(res) = state.borrow().resource_table.get::<ServeResource>(rid) else {
        return Ok(serde_json::Value::Null);
    };
    let mut closed = res.closed.subscribe();
    let msg = {
        let mut rx = res.rx.lock().await;
        tokio::select! {
            msg = rx.recv() => msg,
            _ = closed.wait_for(|c| *c) => None,
        }
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

/// Lines the serve loop prints per window before it starts counting.
const SERVE_LOG_BURST: u32 = 10;
/// The rate-limit window for serve log lines.
const SERVE_LOG_WINDOW: Duration = Duration::from_secs(60);
/// Longest serve log line, in bytes; the script already bounds its parts.
const SERVE_LOG_LINE_MAX: usize = 512;

/// The serve loop's stderr lines, rate-limited here rather than in the
/// script, so the count of suppressed lines lives in the host and is
/// printed at teardown even when the script dies first.
#[derive(Debug, Default)]
pub struct ServeLog {
    window_start: Option<std::time::Instant>,
    printed: u32,
    held: u64,
}

impl ServeLog {
    fn line(&mut self, text: &str) {
        let now = std::time::Instant::now();
        if self
            .window_start
            .is_none_or(|start| now.duration_since(start) >= SERVE_LOG_WINDOW)
        {
            self.flush();
            self.window_start = Some(now);
            self.printed = 0;
        }
        if self.printed < SERVE_LOG_BURST {
            self.printed += 1;
            let mut end = text.len().min(SERVE_LOG_LINE_MAX);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            eprintln!("{}", &text[..end]);
        } else {
            self.held += 1;
        }
    }

    fn flush(&mut self) {
        if self.held > 0 {
            eprintln!("jsrt: serve: {} more such lines suppressed", self.held);
            self.held = 0;
        }
    }
}

/// Print any suppressed-line count still held — called by the runtime
/// when a run ends, however it ends.
pub fn flush_serve_log(state: &mut OpState) {
    if let Some(log) = state.try_borrow_mut::<ServeLog>() {
        log.flush();
    }
}

/// `op_serve_log(line)` — one serve-loop stderr line, rate-limited.
#[op2(fast)]
fn op_serve_log(state: &mut OpState, #[string] line: String) {
    if !state.has::<ServeLog>() {
        state.put(ServeLog::default());
    }
    state.borrow_mut::<ServeLog>().line(&line);
}

/// `op_serve_log_flush()` — a listener closed: report what was held.
#[op2(fast)]
fn op_serve_log_flush(state: &mut OpState) {
    flush_serve_log(state);
}

deno_core::extension!(
    jsrt_serve,
    ops = [
        op_serve_listen,
        op_serve_next,
        op_serve_respond,
        op_serve_close,
        op_serve_log,
        op_serve_log_flush
    ],
);

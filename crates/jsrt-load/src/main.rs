//! `jsrt-load` — the project's own HTTP load driver.
//!
//! Why a driver instead of `ab`/`wrk`: the report has to name the
//! failure shapes honestly — refused connections, timeouts, non-2xx,
//! client-side errors — and emit percentile latency as data, not a
//! prose claim. It drives keep-alive connections the way real traffic
//! does: each worker owns one connection and issues requests serially
//! on it (the same model `rt.serve`'s pull dispatch serves).
//!
//! Output is one JSON object on stdout — machine-readable so
//! perf/results/ artifacts carry the numbers, not a screenshot.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::Request;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

#[derive(Parser)]
struct Args {
    /// URL to hit, e.g. http://127.0.0.1:9400/health
    #[arg(long)]
    url: String,
    /// Total requests to issue across all workers
    #[arg(long)]
    requests: u64,
    /// Concurrent keep-alive connections
    #[arg(long, default_value_t = 16)]
    concurrency: u64,
    /// Per-request timeout
    #[arg(long, default_value_t = 10)]
    timeout_secs: u64,
    /// HTTP method (GET, POST, ...)
    #[arg(long, default_value = "GET")]
    method: String,
    /// Request body — a literal string. Repeatable headers below.
    #[arg(long)]
    body: Option<String>,
    /// Header as "Name: value" — repeat for several.
    #[arg(long = "header")]
    headers: Vec<String>,
}

#[derive(Default, Clone, Copy)]
struct Stats {
    ok: u64,
    non2xx: u64,
    refused: u64,
    timeout: u64,
    client_err: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = Args::parse();
    let url: hyper::Uri = args.url.parse().unwrap_or_else(|e| {
        eprintln!("bad --url {}: {e}", args.url);
        std::process::exit(2);
    });
    let timeout = Duration::from_secs(args.timeout_secs);

    // One atomic ticket counter — workers pull indices until the run
    // is spent. Latency samples go in a per-worker Vec merged at the
    // end; a Mutex<Vec> under 1M pushes is cheaper than it looks
    // (push is uncontended most of the time, batches are per-worker).
    let next = Arc::new(AtomicU64::new(0));
    let samples: Arc<Mutex<Vec<u64>>> =
        Arc::new(Mutex::new(Vec::with_capacity(args.requests as usize)));
    let errors: Arc<Mutex<Stats>> = Arc::new(Mutex::new(Stats::default()));

    let started = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..args.concurrency {
        let next = next.clone();
        let samples = samples.clone();
        let errors = errors.clone();
        let url = url.clone();
        let method = args.method.clone();
        let body = args.body.clone();
        let headers = args.headers.clone();
        handles.push(tokio::spawn(async move {
            let client: Client<_, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build(HttpConnector::new());
            let mut local_samples: Vec<u64> = Vec::with_capacity(4096);
            let mut local = Stats::default();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= args.requests {
                    break;
                }
                let t0 = Instant::now();
                let mut builder = Request::builder().uri(&url).method(method.as_str());
                for h in &headers {
                    if let Some((k, v)) = h.split_once(':') {
                        builder = builder.header(k.trim(), v.trim());
                    }
                }
                let payload = body
                    .as_deref()
                    .map(|b| Bytes::copy_from_slice(b.as_bytes()))
                    .unwrap_or_default();
                let req = builder.body(Full::new(payload)).unwrap_or_else(|e| {
                    eprintln!("build req: {e}");
                    std::process::exit(2);
                });
                match tokio::time::timeout(timeout, client.request(req)).await {
                    Ok(Ok(resp)) => {
                        if resp.status().is_success() {
                            local.ok += 1;
                        } else {
                            local.non2xx += 1;
                        }
                        // Drain the body so the connection stays reusable.
                        let _ = resp.into_body().collect().await;
                    }
                    Ok(Err(e)) => {
                        if e.is_connect() {
                            local.refused += 1;
                        } else {
                            local.client_err += 1;
                        }
                    }
                    Err(_) => local.timeout += 1,
                }
                local_samples.push(t0.elapsed().as_micros() as u64);
                if local_samples.len() == local_samples.capacity() {
                    samples
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .append(&mut local_samples);
                }
            }
            if !local_samples.is_empty() {
                samples
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append(&mut local_samples);
            }
            let mut e = errors.lock().unwrap_or_else(|e| e.into_inner());
            e.ok += local.ok;
            e.non2xx += local.non2xx;
            e.refused += local.refused;
            e.timeout += local.timeout;
            e.client_err += local.client_err;
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let wall = started.elapsed();

    let mut lat = std::mem::take(&mut *samples.lock().unwrap_or_else(|e| e.into_inner()));
    lat.sort_unstable();
    let e = *errors.lock().unwrap_or_else(|e| e.into_inner());
    let n = lat.len() as f64;
    let pct = |q: f64| -> u64 {
        if lat.is_empty() {
            0
        } else {
            lat[((lat.len() as f64 * q) as usize).min(lat.len() - 1)]
        }
    };
    let report = serde_json::json!({
        "url": args.url,
        "method": args.method,
        "requests_target": args.requests,
        "requests_issued": lat.len(),
        "concurrency": args.concurrency,
        "wall_ms": wall.as_millis() as u64,
        "rps": (n / wall.as_secs_f64()).round() as u64,
        "latency_us": {
            "p50": pct(0.50),
            "p95": pct(0.95),
            "p99": pct(0.99),
            "max": lat.last().copied().unwrap_or(0),
        },
        "outcomes": {
            "ok": e.ok,
            "non2xx": e.non2xx,
            "refused": e.refused,
            "timeout": e.timeout,
            "client_err": e.client_err,
        },
        "error_rate": if n > 0.0 { (lat.len() as u64 - e.ok) as f64 / n } else { 0.0 },
    });
    println!(
        "{}",
        serde_json::to_string(&report).unwrap_or_else(|_| "{}".into())
    );
}

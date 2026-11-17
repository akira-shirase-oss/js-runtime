//! Security regression tests: each one proves a defence holds, against the
//! real binary, and fails when that defence is removed.
//!
//! Listeners these tests start take ports from 30000-31999: below the
//! ephemeral ranges of macOS (49152+) and Linux (32768+), so a client
//! socket elsewhere in the run never holds one, and apart from the
//! 20000-29999 range the serve tests use.

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn script(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, source).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        path
    }

    fn run_script(&self, script: &Path, extra: &[&str]) -> Output {
        self.run_script_env(script, extra, &[])
    }

    fn run_script_env(&self, script: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
        let mut args = vec!["run", arg(script)];
        args.extend_from_slice(extra);
        let mut cmd = Command::new(JSRT);
        cmd.args(&args).current_dir(self.dir.path());
        if !env.is_empty() {
            // An inherited NO_PROXY could exempt the destination and hide a
            // proxy that would otherwise have been used.
            cmd.env_remove("NO_PROXY").env_remove("no_proxy");
        }
        cmd.envs(env.iter().copied())
            .output()
            .unwrap_or_else(|e| panic!("spawn jsrt: {e}"))
    }
}

fn arg(path: &Path) -> &str {
    path.to_str().unwrap_or_else(|| panic!("utf8 path"))
}

fn code(out: &Output) -> i32 {
    out.status
        .code()
        .unwrap_or_else(|| panic!("jsrt killed by signal: {:?}", out.status))
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A loopback listener on a port from 30000-31999 that is free right now.
fn listener() -> TcpListener {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let start = std::process::id() as usize % 2_000;
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(n < 2_000, "no free test port in 30000-31999");
        let port = 30_000 + ((start + n) % 2_000) as u16;
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)) {
            return l;
        }
    }
}

/// An HTTP stub that counts every connection it accepts and answers each
/// with `body` (a JavaScript module, so a loader that did fetch it would
/// run it). One thread per connection.
struct CountingStub {
    port: u16,
    connections: Arc<AtomicUsize>,
}

impl CountingStub {
    fn start(body: &'static str) -> Self {
        let listener = listener();
        let port = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("addr: {e}"))
            .port();
        let connections = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&connections);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                seen.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(body.as_bytes());
                });
            }
        });
        Self { port, connections }
    }

    fn seen(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

// ---- Imports: only local files are modules ---------------------------------

/// What a remote module would print if it were ever loaded and run.
const REMOTE_MODULE: &str = "rt.print('REMOTE CODE RAN\\n');\nexport default 1;\n";

#[test]
fn static_remote_and_data_imports_are_refused_by_jsrts_loader() {
    let stub = CountingStub::start(REMOTE_MODULE);
    let rig = Rig::new();
    let specifiers = [
        format!("http://127.0.0.1:{}/m.js", stub.port),
        format!("https://127.0.0.1:{}/m.js", stub.port),
        "data:text/javascript,rt.print(\"DATA CODE RAN\")".to_string(),
    ];
    for (i, spec) in specifiers.iter().enumerate() {
        let script = rig.script(&format!("static{i}.js"), &format!("import '{spec}';\n"));
        // Even a grant for the stub's address does not make it a module
        // source: `net` is for rt.fetch, not for code.
        let out = rig.run_script(&script, &["--net", &format!("127.0.0.1:{}", stub.port)]);
        assert_eq!(code(&out), 7, "{spec}: stderr: {}", stderr(&out));
        assert!(
            stderr(&out).contains("import denied:"),
            "{spec}: jsrt's own loader refuses it; stderr: {}",
            stderr(&out)
        );
        assert!(!stdout(&out).contains("RAN"), "{spec}: {}", stdout(&out));
    }
    assert_eq!(stub.seen(), 0, "the loader must never connect anywhere");
}

#[test]
fn dynamic_remote_data_and_blob_imports_are_refused_by_jsrts_loader() {
    let stub = CountingStub::start(REMOTE_MODULE);
    let rig = Rig::new();
    let specs = format!(
        "['http://127.0.0.1:{port}/m.js', 'https://127.0.0.1:{port}/m.js', \
          'data:text/javascript,rt.print(\"DATA CODE RAN\")', \
          'blob:http://127.0.0.1:{port}/0d3f']",
        port = stub.port
    );
    let script = rig.script(
        "dynamic.js",
        &format!(
            "for (const spec of {specs}) {{\n\
               try {{ await import(spec); rt.print('loaded ' + spec + '\\n'); }}\n\
               catch (e) {{ rt.print((String(e.message).includes('import denied:') ? 'refused' : 'other: ' + e.message) + '\\n'); }}\n\
             }}\n\
             await import('http://127.0.0.1:{port}/uncaught.js');\n",
            port = stub.port
        ),
    );
    let out = rig.run_script(&script, &["--net", &format!("127.0.0.1:{}", stub.port)]);
    assert_eq!(
        stdout(&out),
        "refused\nrefused\nrefused\nrefused\n",
        "stderr: {}",
        stderr(&out)
    );
    // The last one is not caught: a refusal, not a load failure.
    assert_eq!(code(&out), 7, "stderr: {}", stderr(&out));
    assert_eq!(stub.seen(), 0, "the loader must never connect anywhere");
}

// ---- rt.fetch never goes through an environment proxy -----------------------

#[test]
fn fetch_ignores_proxy_environment_variables() {
    let proxy = CountingStub::start("from the proxy");
    let target = CountingStub::start("from the target");
    let rig = Rig::new();
    let proxy_url = format!("http://127.0.0.1:{}", proxy.port);
    let env: Vec<(&str, &str)> = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
    .into_iter()
    .map(|k| (k, proxy_url.as_str()))
    .collect();
    let script = rig.script(
        "proxy.js",
        &format!(
            "const r = await rt.fetch('http://127.0.0.1:{port}/plain');\n\
             rt.print('http: ' + r.body + '\\n');\n\
             // The target speaks plain HTTP, so a direct https connection\n\
             // fails its TLS handshake; through a proxy it would be a CONNECT.\n\
             try {{ await rt.fetch('https://127.0.0.1:{port}/tls'); rt.print('https: answered\\n'); }}\n\
             catch (e) {{ rt.print('https: ' + e.name + '\\n'); }}\n",
            port = target.port
        ),
    );
    let out = rig.run_script_env(
        &script,
        &["--net", &format!("127.0.0.1:{}", target.port)],
        &env,
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "http: from the target\nhttps: HostError\n",
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(proxy.seen(), 0, "the proxy must never be contacted");
    assert_eq!(target.seen(), 2, "both fetches went straight to the target");
}

// ---- rt.fetch: a bounded number of exchanges in flight ---------------------

/// Most fetches one run has in flight; the rest wait for a slot.
const FETCH_MAX_IN_FLIGHT: usize = 32;

#[test]
fn a_thousand_parallel_fetches_keep_in_flight_under_the_cap() {
    // A stub that holds every request for 50 ms and records how many it
    // was holding at once.
    let listener = listener();
    let port = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("addr: {e}"))
        .port();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (now, high) = (Arc::clone(&in_flight), Arc::clone(&peak));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            let (now, high) = (Arc::clone(&now), Arc::clone(&high));
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                if s.read(&mut buf).unwrap_or(0) == 0 {
                    return;
                }
                let held = now.fetch_add(1, Ordering::SeqCst) + 1;
                high.fetch_max(held, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(50));
                now.fetch_sub(1, Ordering::SeqCst);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            });
        }
    });
    let rig = Rig::new();
    let script = rig.script(
        "parallel.js",
        &format!(
            "const all = [];\n\
             for (let i = 0; i < 1000; i++) all.push(rt.fetch('http://127.0.0.1:{port}/' + i));\n\
             const results = await Promise.allSettled(all);\n\
             const ok = results.filter((r) => r.status === 'fulfilled' && r.value.body === 'ok').length;\n\
             rt.print('ok ' + ok + '\\n');\n"
        ),
    );
    let out = rig.run_script(
        &script,
        &["--net", &format!("127.0.0.1:{port}"), "--timeout", "60000"],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "ok 1000\n", "stderr: {}", stderr(&out));
    let peak = peak.load(Ordering::SeqCst);
    assert!(
        (1..=FETCH_MAX_IN_FLIGHT).contains(&peak),
        "{peak} requests were in flight at once; the cap is {FETCH_MAX_IN_FLIGHT}"
    );
}

// ---- Memory: the V8 heap has a limit that ends a run, not the process ------

#[test]
fn an_allocation_bomb_under_jsrt_run_ends_classified_at_the_heap_limit() {
    let rig = Rig::new();
    let script = rig.script(
        "bomb.js",
        "const hoard = [];\n\
         while (true) { hoard.push(new Array(100000).fill({ x: hoard.length })); }\n",
    );
    let started = std::time::Instant::now();
    let out = rig.run_script(&script, &["--max-heap-mb", "64", "--timeout", "60000"]);
    let took = started.elapsed();
    // A V8 out-of-memory abort is a signal (SIGTRAP or SIGABRT), not an
    // exit code; the host must end the run itself and say why.
    assert_eq!(
        out.status.code(),
        Some(6),
        "status {:?} after {took:?}, stderr: {}",
        out.status,
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("heap limit"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        took < std::time::Duration::from_secs(30),
        "the heap limit, not the run timeout, ended it: {took:?}"
    );
}
